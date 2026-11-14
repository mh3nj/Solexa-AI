import { useState } from "react";
import { addDecision, addKnowledgeItem, archiveKnowledgeItem, resolveDecision, sendDecisionToGithub } from "../lib/api";
import type { DecisionDto, KnowledgeItemDto, KnowledgeKind, MemberDto } from "../lib/types";
import { useI18n } from "../i18n";

// The Project Brain (Phase 4/6): facts/constraints/artifacts a task's
// ContextPackage gets scoped from by tag match, plus Decisions, a
// structured record of "question, options, resolution, approver" (spec
// section 10) rather than a fact that's simply true or not. Collapsed
// by default so it doesn't compete with the task list for attention.
//
// "Archived" items are hidden from the default view but never deleted
// spec rule: uncertain/outdated project knowledge gets a status
// change, not erasure. What reads as "remove" here is that status
// change, not a DELETE.
export function ProjectBrainPanel({
  items,
  decisions,
  members,
  onChanged,
}: {
  items: KnowledgeItemDto[];
  decisions: DecisionDto[];
  members: MemberDto[];
  onChanged: () => void;
}) {
  const { t } = useI18n();
  const activeItems = items.filter((i) => i.status !== "archived");
  const itemLabel = t(activeItems.length === 1 ? "brain.itemsOne" : "brain.itemsMany", {
    count: activeItems.length,
  });
  const decisionLabel = t(decisions.length === 1 ? "brain.decisionsOne" : "brain.decisionsMany", {
    count: decisions.length,
  });

  return (
    <details className="rounded-lg border border-line bg-panel">
      <summary className="cursor-pointer select-none px-4 py-3 text-xs font-mono uppercase tracking-wide text-muted">
        {t("brain.summary", { items: itemLabel, decisions: decisionLabel })}
      </summary>
      <div className="space-y-4 px-4 pb-4">
        <div>
          {activeItems.length > 0 && (
            <ul className="mb-3 space-y-1.5">
              {activeItems.map((item) => (
                <li key={item.id} className="rounded border border-line bg-graphite/60 px-3 py-2">
                  <div className="flex items-center justify-between gap-2">
                    <p className="text-xs font-medium text-ink">{item.title}</p>
                    <div className="flex shrink-0 items-center gap-1.5">
                      <span className="rounded bg-line/50 px-1.5 py-0.5 font-mono text-[10px] text-muted">
                        {t(`brain.${item.kind}`)}
                      </span>
                      <button
                        onClick={() => archiveKnowledgeItem(item.id).then(onChanged).catch(console.error)}
                        className="text-[10px] text-muted transition hover:text-rose"
                        title={t("brain.archiveHint")}
                      >
                        {t("brain.archive")}
                      </button>
                    </div>
                  </div>
                  <p className="mt-0.5 text-[11px] text-muted">{item.content}</p>
                  {item.tags.length > 0 && (
                    <div className="mt-1 flex flex-wrap gap-1">
                      {item.tags.map((tag) => (
                        <span key={tag} className="rounded bg-line/30 px-1 py-0.5 font-mono text-[10px] text-muted">
                          {tag}
                        </span>
                      ))}
                    </div>
                  )}
                </li>
              ))}
            </ul>
          )}
          <AddKnowledgeForm onAdded={onChanged} />
        </div>

        <div className="border-t border-line pt-4">
          {decisions.length > 0 && (
            <ul className="mb-3 space-y-1.5">
              {decisions.map((decision) => (
                <DecisionRow key={decision.id} decision={decision} members={members} onResolved={onChanged} />
              ))}
            </ul>
          )}
          <AddDecisionForm onAdded={onChanged} />
        </div>
      </div>
    </details>
  );
}

function DecisionRow({
  decision,
  members,
  onResolved,
}: {
  decision: DecisionDto;
  members: MemberDto[];
  onResolved: () => void;
}) {
  const { t } = useI18n();
  const [chosenOption, setChosenOption] = useState(decision.options[0] ?? "");
  const [approverId, setApproverId] = useState(members[0]?.id ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [githubBusy, setGithubBusy] = useState(false);
  const [githubError, setGithubError] = useState<string | null>(null);

  async function sendToGithub() {
    setGithubBusy(true);
    setGithubError(null);
    try {
      await sendDecisionToGithub(decision.id);
      onResolved();
    } catch (err) {
      setGithubError(err instanceof Error ? err.message : String(err));
    } finally {
      setGithubBusy(false);
    }
  }

  async function resolve() {
    if (!chosenOption || !approverId || busy) return;
    setBusy(true);
    setError(null);
    try {
      await resolveDecision(decision.id, chosenOption, approverId);
      onResolved();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <li className="rounded border border-line bg-graphite/60 px-3 py-2">
      <div className="flex items-center justify-between gap-2">
        <p className="text-xs font-medium text-ink">{decision.question}</p>
        <span
          className={`shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] ${
            decision.status === "resolved" ? "bg-teal/10 text-teal" : "bg-amber/10 text-amber"
          }`}
        >
          {decision.status === "resolved" ? t("brain.status.resolved") : t("brain.status.open")}
        </span>
      </div>
      {decision.status === "resolved" ? (
        <p className="mt-1 text-[11px] text-muted">
          {t("brain.resolved")} <span className="text-teal">{decision.resolution}</span> ·{" "}
          {t("brain.approvedBy", { name: decision.approved_by ?? "" })}
        </p>
      ) : (
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          <select
            value={chosenOption}
            onChange={(e) => setChosenOption(e.target.value)}
            className="rounded border border-line bg-graphite px-1.5 py-0.5 text-[11px] text-ink"
          >
            {decision.options.map((opt) => (
              <option key={opt} value={opt}>
                {opt}
              </option>
            ))}
          </select>
          <select
            value={approverId}
            onChange={(e) => setApproverId(e.target.value)}
            className="rounded border border-line bg-graphite px-1.5 py-0.5 text-[11px] text-ink"
          >
            {members.map((m) => (
              <option key={m.id} value={m.id}>
                {m.display_name}
              </option>
            ))}
          </select>
          <button
            onClick={resolve}
            disabled={busy}
            className="rounded bg-amber/90 px-2 py-0.5 text-[11px] font-medium text-onaccent transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
          >
            {t("brain.resolve")}
          </button>
        </div>
      )}
      {error && <p className="mt-1 text-[11px] text-rose">{error}</p>}
      <div className="mt-1.5 border-t border-line/60 pt-1.5">
        {decision.github_issue_url ? (
          <a
            href={decision.github_issue_url}
            target="_blank"
            rel="noreferrer"
            className="text-[10px] text-teal underline decoration-dotted transition hover:text-ink"
          >
            {t("brain.viewIssue")}
          </a>
        ) : (
          <button
            onClick={sendToGithub}
            disabled={githubBusy}
            className="text-[10px] text-teal underline decoration-dotted transition hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
          >
            {t("brain.sendIssue")}
          </button>
        )}
        {githubError && <p className="mt-1 text-[11px] text-rose">{githubError}</p>}
      </div>
    </li>
  );
}

function AddKnowledgeForm({ onAdded }: { onAdded: () => void }) {
  const { t } = useI18n();
  const [kind, setKind] = useState<KnowledgeKind>("constraint");
  const [title, setTitle] = useState("");
  const [content, setContent] = useState("");
  const [tags, setTags] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!title.trim() || !content.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      await addKnowledgeItem(
        kind,
        title.trim(),
        content.trim(),
        tags.split(",").map((t) => t.trim()).filter(Boolean),
      );
      setTitle("");
      setContent("");
      setTags("");
      onAdded();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={handleSubmit} className="space-y-2">
      <p className="text-[11px] font-mono uppercase tracking-wide text-muted">
        {t("brain.addKnowledge")}
      </p>
      <div className="flex gap-2">
        <select
          value={kind}
          onChange={(e) => setKind(e.target.value as KnowledgeKind)}
          className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink"
        >
          <option value="constraint">{t("brain.constraint")}</option>
          <option value="fact">{t("brain.fact")}</option>
          <option value="artifact">{t("brain.artifact")}</option>
        </select>
        <input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder={t("brain.titleField")}
          className="flex-1 rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
        />
      </div>
      <textarea
        value={content}
        onChange={(e) => setContent(e.target.value)}
        placeholder={t("brain.contentField")}
        rows={2}
        className="w-full resize-none rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
      />
      <div className="flex items-center gap-2">
        <input
          value={tags}
          onChange={(e) => setTags(e.target.value)}
          placeholder={t("brain.tagsField")}
          className="flex-1 rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
        />
        <button
          type="submit"
          disabled={busy || !title.trim() || !content.trim()}
          className="shrink-0 rounded bg-amber/90 px-3 py-1 text-xs font-medium text-onaccent transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
        >
          {t("common.add")}
        </button>
      </div>
      {error && <p className="text-[11px] text-rose">{error}</p>}
      <p className="text-[10px] text-muted">{t("brain.tagsHelp")}</p>
    </form>
  );
}

function AddDecisionForm({ onAdded }: { onAdded: () => void }) {
  const { t } = useI18n();
  const [question, setQuestion] = useState("");
  const [optionsText, setOptionsText] = useState("");
  const [tags, setTags] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const options = optionsText.split(",").map((o) => o.trim()).filter(Boolean);
    if (!question.trim() || options.length < 2 || busy) {
      setError(t("brain.validation.decision"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await addDecision(
        question.trim(),
        options,
        tags.split(",").map((t) => t.trim()).filter(Boolean),
      );
      setQuestion("");
      setOptionsText("");
      setTags("");
      onAdded();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={handleSubmit} className="space-y-2">
      <p className="text-[11px] font-mono uppercase tracking-wide text-muted">
        {t("brain.addDecision")}
      </p>
      <input
        value={question}
        onChange={(e) => setQuestion(e.target.value)}
        placeholder={t("brain.questionField")}
        className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
      />
      <input
        value={optionsText}
        onChange={(e) => setOptionsText(e.target.value)}
        placeholder={t("brain.optionsField")}
        className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
      />
      <div className="flex items-center gap-2">
        <input
          value={tags}
          onChange={(e) => setTags(e.target.value)}
          placeholder={t("brain.tagsShort")}
          className="flex-1 rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
        />
        <button
          type="submit"
          disabled={busy}
          className="shrink-0 rounded bg-amber/90 px-3 py-1 text-xs font-medium text-onaccent transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
        >
          {t("common.add")}
        </button>
      </div>
      {error && <p className="text-[11px] text-rose">{error}</p>}
    </form>
  );
}
