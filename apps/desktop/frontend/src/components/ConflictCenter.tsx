import { useState } from "react";
import { resolveConflict, resolveSyncConflict, sendConflictToGithub, checkConflictGithubPrStatus } from "../lib/api";
import type { ConflictDto, MemberDto } from "../lib/types";
import { useI18n } from "../i18n";

// A conflict is never hidden or auto-resolved. Someone has to pick a
// position and name who approved it.
//
// Sync conflicts reuse the same shape as task conflicts. When
// `sync_object` is set, the card shows which field diverged and offers
// the GitHub pull request actions.
export function ConflictCenter({
  conflicts,
  members,
  onResolved,
}: {
  conflicts: ConflictDto[];
  members: MemberDto[];
  onResolved: () => void;
}) {
  const { t } = useI18n();
  const open = conflicts.filter((c) => c.status === "open");
  if (open.length === 0) return null;

  return (
    <div className="rounded-lg border border-rose/40 bg-rose/5 p-4">
      <p className="mb-3 text-xs font-mono uppercase tracking-wide text-rose">
        {t(open.length > 1 ? "conflicts.openMany" : "conflicts.openOne", { count: open.length })}
      </p>
      <div className="space-y-4">
        {open.map((conflict) => (
          <ConflictCard key={conflict.id} conflict={conflict} members={members} onResolved={onResolved} />
        ))}
      </div>
    </div>
  );
}

function ConflictCard({
  conflict,
  members,
  onResolved,
}: {
  conflict: ConflictDto;
  members: MemberDto[];
  onResolved: () => void;
}) {
  const { t } = useI18n();
  const [approverId, setApproverId] = useState(members[0]?.id ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [githubBusy, setGithubBusy] = useState(false);
  const [githubError, setGithubError] = useState<string | null>(null);
  const [lastCheckResult, setLastCheckResult] = useState<string | null>(null);

  async function accept(specialistId: string) {
    if (!approverId) {
      setError(t("conflicts.chooseApprover"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      if (conflict.sync_object) {
        await resolveSyncConflict(conflict.id, specialistId, approverId);
      } else {
        await resolveConflict(conflict.id, specialistId, approverId);
      }
      onResolved();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function sendToGithub() {
    setGithubBusy(true);
    setGithubError(null);
    try {
      await sendConflictToGithub(conflict.id);
      onResolved();
    } catch (err) {
      setGithubError(err instanceof Error ? err.message : String(err));
    } finally {
      setGithubBusy(false);
    }
  }

  async function checkGithubStatus() {
    if (!approverId) {
      setGithubError(t("conflicts.chooseApproverForPr"));
      return;
    }
    setGithubBusy(true);
    setGithubError(null);
    try {
      const outcome = await checkConflictGithubPrStatus(conflict.id, approverId);
      setLastCheckResult(outcome);
      onResolved();
    } catch (err) {
      setGithubError(err instanceof Error ? err.message : String(err));
    } finally {
      setGithubBusy(false);
    }
  }

  const kindLabel = conflict.sync_object
    ? `${t("conflicts.syncConflict")}: ${conflict.sync_object.kind}${
        conflict.sync_object.field ? `.${conflict.sync_object.field}` : ""
      }`
    : t("conflicts.taskConflict");

  return (
    <div className="rounded-md border border-line bg-panel p-3">
      <p className="text-[10px] uppercase tracking-wide text-muted">{kindLabel}</p>
      <p className="text-sm text-ink">{conflict.question}</p>

      <div className="mt-3 grid gap-2 sm:grid-cols-2">
        {conflict.positions.map((position) => (
          <div key={position.specialist_id} className="rounded border border-line bg-graphite/60 p-2">
            <div className="flex items-center justify-between gap-2">
              <p className="truncate text-xs font-medium text-ink">{position.specialist_name}</p>
              {position.verification_passed !== null && (
                <span className={`shrink-0 text-[10px] ${position.verification_passed ? "text-teal" : "text-rose"}`}>
                  {position.verification_passed ? t("conflicts.verified") : t("conflicts.failedVerification")}
                </span>
              )}
            </div>
            <pre className="mt-1 max-h-24 overflow-auto whitespace-pre-wrap font-mono text-[11px] text-muted">
              {typeof position.content === "string" ? position.content : JSON.stringify(position.content, null, 2)}
            </pre>
            <button
              onClick={() => accept(position.specialist_id)}
              disabled={busy}
              className="mt-2 w-full rounded bg-amber/90 px-2 py-1 text-[11px] font-medium text-graphite transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("conflicts.acceptLocal")}
            </button>
          </div>
        ))}
      </div>

      <div className="mt-3 flex items-center gap-2">
        <label htmlFor={`approver-${conflict.id}`} className="text-[11px] text-muted">
          {t("conflicts.approvedBy")}
        </label>
        <select
          id={`approver-${conflict.id}`}
          value={approverId}
          onChange={(e) => setApproverId(e.target.value)}
          className="rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
        >
          {members.map((m) => (
            <option key={m.id} value={m.id}>
              {m.display_name}
            </option>
          ))}
        </select>
      </div>

      {conflict.sync_object && (
        <div className="mt-3 border-t border-line/60 pt-2">
          {conflict.github_pr_url ? (
            <div className="flex flex-wrap items-center gap-2">
              <a
                href={conflict.github_pr_url}
                target="_blank"
                rel="noreferrer"
                className="text-[11px] text-teal underline decoration-dotted transition hover:text-ink"
              >
                {t("conflicts.viewPr")}
              </a>
              <button
                onClick={checkGithubStatus}
                disabled={githubBusy}
                className="rounded border border-line px-2 py-1 text-[11px] text-ink transition hover:border-teal disabled:cursor-not-allowed disabled:opacity-40"
              >
                {t("conflicts.checkPr")}
              </button>
              {lastCheckResult === "still_open" && (
                <span className="text-[10px] text-muted">{t("conflicts.stillOpen")}</span>
              )}
            </div>
          ) : (
            <button
              onClick={sendToGithub}
              disabled={githubBusy}
              className="rounded border border-line px-2 py-1 text-[11px] text-ink transition hover:border-teal disabled:cursor-not-allowed disabled:opacity-40"
            >
              {t("conflicts.sendPr")}
            </button>
          )}
          <p className="mt-1 text-[10px] text-muted">{t("conflicts.help")}</p>
          {githubError && <p className="mt-1 text-[11px] text-rose">{githubError}</p>}
        </div>
      )}

      {error && <p className="mt-2 text-[11px] text-rose">{error}</p>}
    </div>
  );
}
