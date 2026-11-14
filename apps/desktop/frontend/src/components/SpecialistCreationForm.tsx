import { useEffect, useRef, useState } from "react";
import { createSpecialist, listModels, updateSpecialist } from "../lib/api";
import type { ModelDto, PermissionName, SpecialistDto, TeamDto } from "../lib/types";
import { useI18n } from "../i18n";

// Spec section 13, "Specialist Creation": a structured flow (name,
// role, domain, capabilities, models, tools, knowledge scope,
// permissions, output format, escalation rules), explicitly "not a
// giant system-prompt textbox." Every field here maps to one concrete
// part of the engine's `Specialist`/`EscalationPolicy`, nothing here
// is free text standing in for configuration the engine doesn't
// actually have a place for.
//
// This one component covers both creating a new specialist and editing
// an existing one (`editingSpecialist` set), the structured shape is
// identical either way, only what happens on submit and which fields
// are shown differs:
//   - Creating also creates the AI `Member` that runs the new
//     specialist (spec: "MODEL != MEMBER") and, if a team is picked,
//     adds that member to it, otherwise the specialist would be
//     immediately routable but invisible in the Team rail.
//   - Editing never touches the member/team wiring `create_specialist`
//     set up, and never touches `active`, archiving is a separate,
//     deliberate action (see `SpecialistRail`'s archive/reactivate
//     control), never a side effect of saving an unrelated field edit.

const ALL_PERMISSIONS: PermissionName[] = ["read", "propose", "edit", "review", "approve", "execute"];

export function SpecialistCreationForm({
  teams,
  editingSpecialist,
  onChanged,
  onCancelEdit,
  modelsRefreshSignal,
}: {
  teams: TeamDto[];
  editingSpecialist?: SpecialistDto | null;
  onChanged: () => void;
  onCancelEdit?: () => void;
  // Round 45: bump this (any changing number, a parent-owned counter)
  // whenever the available-models list may have changed elsewhere
  // (e.g. ModelsPanel enabling/disabling one) so an already-open
  // picker doesn't go stale. Optional and unused if the parent never
  // passes it, the effect below still runs once on mount either way.
  modelsRefreshSignal?: number;
}) {
  const { t } = useI18n();
  const isEditing = Boolean(editingSpecialist);

  const [panelOpen, setPanelOpen] = useState(false);
  const [models, setModels] = useState<ModelDto[]>([]);
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [domain, setDomain] = useState("");
  const [capabilities, setCapabilities] = useState("");
  const [selectedModelIds, setSelectedModelIds] = useState<string[]>([]);
  const [tools, setTools] = useState("");
  const [knowledgeScope, setKnowledgeScope] = useState("");
  const [permissions, setPermissions] = useState<PermissionName[]>(["read", "propose", "execute"]);
  const [outputSchema, setOutputSchema] = useState("");
  const [escalateOnWeakConfidence, setEscalateOnWeakConfidence] = useState(true);
  const [escalateOnVerificationFailure, setEscalateOnVerificationFailure] = useState(true);
  const [requiresModelReview, setRequiresModelReview] = useState(false);
  const [requiresSpecialistReview, setRequiresSpecialistReview] = useState(false);
  // Round 52: kept as the raw field text so "" (unset) and "0" stay
  // distinguishable while typing; parsed and validated on submit.
  const [maxTokens, setMaxTokens] = useState("");
  const [temperature, setTemperature] = useState("");
  const [memberDisplayName, setMemberDisplayName] = useState("");
  const [teamId, setTeamId] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The "pick a sensible default" behavior below (select the first
  // available model when creating a brand-new specialist) is meant to
  // fire exactly once, the first time this form ever loads a model
  // list, never again on a later refetch, so it can never silently
  // overwrite a selection the user already made (or clobber a model
  // that's since become unselectable) just because the list refreshed.
  const hasAutoSelectedModel = useRef(false);

  useEffect(() => {
    listModels().then((fetched) => {
      setModels(fetched);
      if (!isEditing && !hasAutoSelectedModel.current && fetched.length > 0) {
        setSelectedModelIds([fetched[0].id]);
        hasAutoSelectedModel.current = true;
      }
    }).catch(console.error);
    // Round 45, closing round 41's own named gap: refetch whenever
    // `modelsRefreshSignal` changes, not just on mount, otherwise a
    // model enabled/disabled in ModelsPanel while this form is already
    // open never shows up here until the whole app reloads. Toggling a
    // model already selected (here, or as a specialist's own stored
    // preference in edit mode) off doesn't clear `selectedModelIds`,
    // it just stops rendering a checkbox for it, the same
    // "preference order, not a hard requirement" behavior the edit
    // flow already relied on before this round.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [modelsRefreshSignal]);

  useEffect(() => {
    if (!teamId && teams.length > 0) setTeamId(teams[0].id);
  }, [teams, teamId]);

  // Pre-fill (or clear) the form whenever which specialist is being
  // edited changes, and force the panel open so the edit is visible
  // without an extra click.
  useEffect(() => {
    if (!editingSpecialist) return;
    setPanelOpen(true);
    setName(editingSpecialist.name);
    setRole(editingSpecialist.role);
    setDomain(editingSpecialist.domain);
    setCapabilities(editingSpecialist.capabilities.join(", "));
    setSelectedModelIds(editingSpecialist.preferred_model_ids ?? []);
    setTools((editingSpecialist.tools ?? []).join(", "));
    setKnowledgeScope((editingSpecialist.knowledge_scope ?? []).join(", "));
    setPermissions(editingSpecialist.permissions ?? []);
    setOutputSchema(
      editingSpecialist.output_schema ? JSON.stringify(editingSpecialist.output_schema, null, 2) : "",
    );
    setEscalateOnWeakConfidence(editingSpecialist.escalate_on_weak_confidence ?? true);
    setEscalateOnVerificationFailure(editingSpecialist.escalate_on_verification_failure ?? true);
    setRequiresModelReview(editingSpecialist.requires_model_review ?? false);
    setRequiresSpecialistReview(editingSpecialist.requires_specialist_review ?? false);
    setMaxTokens(editingSpecialist.max_tokens != null ? String(editingSpecialist.max_tokens) : "");
    setTemperature(editingSpecialist.temperature != null ? String(editingSpecialist.temperature) : "");
    setError(null);
  }, [editingSpecialist]);

  function togglePermission(p: PermissionName) {
    setPermissions((prev) => (prev.includes(p) ? prev.filter((x) => x !== p) : [...prev, p]));
  }

  function toggleModel(id: string) {
    setSelectedModelIds((prev) => (prev.includes(id) ? prev.filter((x) => x !== id) : [...prev, id]));
  }

  function reset() {
    setName("");
    setRole("");
    setDomain("");
    setCapabilities("");
    setTools("");
    setKnowledgeScope("");
    setOutputSchema("");
    setMaxTokens("");
    setTemperature("");
    setMemberDisplayName("");
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const parsedCapabilities = capabilities.split(",").map((c) => c.trim()).filter(Boolean);
    if (!name.trim() || parsedCapabilities.length === 0 || busy) {
      setError(t("specialist.validation.name"));
      return;
    }
    // Round 52: validate here rather than silently coercing, "abc" or
    // "-5" quietly becoming "unset" would leave the user believing a limit
    // was applied that wasn't.
    let parsedMaxTokens: number | undefined;
    if (maxTokens.trim()) {
      const n = Number(maxTokens);
      if (!Number.isInteger(n) || n < 1) {
        setError(t("specialist.validation.maxTokens"));
        return;
      }
      parsedMaxTokens = n;
    }
    let parsedTemperature: number | undefined;
    if (temperature.trim()) {
      const value = Number(temperature);
      if (!Number.isFinite(value) || value < 0 || value > 2) {
        setError(t("specialist.validation.temperature"));
        return;
      }
      parsedTemperature = value;
    }
    setBusy(true);
    setError(null);
    try {
      if (isEditing && editingSpecialist) {
        await updateSpecialist({
          specialistId: editingSpecialist.id,
          name: name.trim(),
          role: role.trim(),
          domain: domain.trim(),
          capabilities: parsedCapabilities,
          preferredModelIds: selectedModelIds,
          tools: tools.split(",").map((t) => t.trim()).filter(Boolean),
          knowledgeScope: knowledgeScope.split(",").map((k) => k.trim()).filter(Boolean),
          permissions,
          outputSchema: outputSchema.trim() || undefined,
          escalateOnWeakConfidence,
          escalateOnVerificationFailure,
          requiresModelReview,
          requiresSpecialistReview,
          maxTokens: parsedMaxTokens,
          temperature: parsedTemperature,
        });
      } else {
        await createSpecialist({
          name: name.trim(),
          role: role.trim(),
          domain: domain.trim(),
          capabilities: parsedCapabilities,
          preferredModelIds: selectedModelIds,
          tools: tools.split(",").map((t) => t.trim()).filter(Boolean),
          knowledgeScope: knowledgeScope.split(",").map((k) => k.trim()).filter(Boolean),
          permissions,
          outputSchema: outputSchema.trim() || undefined,
          escalateOnWeakConfidence,
          escalateOnVerificationFailure,
          requiresModelReview,
          requiresSpecialistReview,
          maxTokens: parsedMaxTokens,
          temperature: parsedTemperature,
          memberDisplayName: memberDisplayName.trim(),
          addToTeamId: teamId || undefined,
        });
        reset();
      }
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="rounded-lg border border-line bg-panel">
      <button
        type="button"
        onClick={() => setPanelOpen((v) => !v)}
        className="w-full cursor-pointer select-none px-4 py-3 text-left text-xs font-mono uppercase tracking-wide text-muted"
      >
        {isEditing && editingSpecialist
          ? t("specialist.editing", { name: editingSpecialist.name })
          : t("specialist.new")}
      </button>
      {panelOpen && (
        <form onSubmit={handleSubmit} className="space-y-3 px-4 pb-4">
          <div className="grid grid-cols-3 gap-2">
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("specialist.name")}
              className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
            />
            <input
              value={role}
              onChange={(e) => setRole(e.target.value)}
              placeholder={t("specialist.role")}
              className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
            />
            <input
              value={domain}
              onChange={(e) => setDomain(e.target.value)}
              placeholder={t("specialist.domain")}
              className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
            />
          </div>

          <input
            value={capabilities}
            onChange={(e) => setCapabilities(e.target.value)}
            placeholder={t("specialist.capabilities")}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
          />

          <div>
            <p className="mb-1 text-[10px] font-mono uppercase tracking-wide text-muted">
              {t("specialist.preferredModels")} {models.length === 0 && t("specialist.noModels")}
            </p>
            <div className="flex flex-wrap gap-2">
              {models.map((m) => (
                <label key={m.id} className="flex items-center gap-1 text-[11px] text-muted">
                  <input
                    type="checkbox"
                    checked={selectedModelIds.includes(m.id)}
                    onChange={() => toggleModel(m.id)}
                    className="accent-amber"
                  />
                  {m.name}
                </label>
              ))}
            </div>
            <p className="mt-0.5 text-[10px] text-muted">{t("specialist.preferredModelsHelp")}</p>
          </div>

          <div className="grid grid-cols-2 gap-2">
            <input
              value={tools}
              onChange={(e) => setTools(e.target.value)}
              placeholder={t("specialist.tools")}
              className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
            />
            <input
              value={knowledgeScope}
              onChange={(e) => setKnowledgeScope(e.target.value)}
              placeholder={t("specialist.knowledgeScope")}
              className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
            />
          </div>

          <div>
            <p className="mb-1 text-[10px] font-mono uppercase tracking-wide text-muted">
              {t("specialist.permissions")}
            </p>
            <div className="flex flex-wrap gap-2">
              {ALL_PERMISSIONS.map((p) => (
                <label key={p} className="flex items-center gap-1 text-[11px] text-muted">
                  <input
                    type="checkbox"
                    checked={permissions.includes(p)}
                    onChange={() => togglePermission(p)}
                    className="accent-amber"
                  />
                  {t(`permission.${p}`)}
                </label>
              ))}
            </div>
            <p className="mt-0.5 text-[10px] text-muted">{t("specialist.permissionsHelp")}</p>
            {!permissions.includes("execute") && (
              <p className="mt-0.5 text-[10px] text-amber">{t("specialist.withoutExecute")}</p>
            )}
          </div>

          <textarea
            value={outputSchema}
            onChange={(e) => setOutputSchema(e.target.value)}
            placeholder={t("specialist.outputSchema")}
            rows={2}
            className="w-full resize-none rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink placeholder:text-muted/60"
          />

          <div>
            <p className="mb-1 text-[10px] font-mono uppercase tracking-wide text-muted">
              {t("specialist.escalation")}
            </p>
            <div className="flex flex-wrap gap-3">
              <label className="flex items-center gap-1 text-[11px] text-muted">
                <input
                  type="checkbox"
                  checked={escalateOnWeakConfidence}
                  onChange={(e) => setEscalateOnWeakConfidence(e.target.checked)}
                  className="accent-amber"
                />
                {t("specialist.escalateWeak")}
              </label>
              <label className="flex items-center gap-1 text-[11px] text-muted">
                <input
                  type="checkbox"
                  checked={escalateOnVerificationFailure}
                  onChange={(e) => setEscalateOnVerificationFailure(e.target.checked)}
                  className="accent-amber"
                />
                {t("specialist.escalateFailure")}
              </label>
              <label className="flex items-center gap-1 text-[11px] text-muted">
                <input
                  type="checkbox"
                  checked={requiresModelReview}
                  onChange={(e) => setRequiresModelReview(e.target.checked)}
                  className="accent-amber"
                />
                {t("specialist.modelReview")}
              </label>
              <label className="flex items-center gap-1 text-[11px] text-muted">
                <input
                  type="checkbox"
                  checked={requiresSpecialistReview}
                  onChange={(e) => setRequiresSpecialistReview(e.target.checked)}
                  className="accent-amber"
                />
                {t("specialist.specialistReview")}
              </label>
            </div>
          </div>

          <div>
            <p className="mb-1 text-[10px] font-mono uppercase tracking-wide text-muted">
              {t("specialist.limits")}
            </p>
            <div className="flex flex-wrap items-center gap-3">
              <label className="flex items-center gap-1 text-[11px] text-muted">
                {t("specialist.maxTokens")}
                <input
                  value={maxTokens}
                  onChange={(e) => setMaxTokens(e.target.value)}
                  inputMode="numeric"
                  placeholder={t("specialist.maxTokensPlaceholder")}
                  aria-label={t("specialist.maxTokens")}
                  className="w-24 rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink placeholder:text-muted/60"
                />
              </label>
              <label className="flex items-center gap-1 text-[11px] text-muted">
                {t("specialist.temperature")}
                <input
                  value={temperature}
                  onChange={(e) => setTemperature(e.target.value)}
                  inputMode="decimal"
                  placeholder={t("specialist.temperaturePlaceholder")}
                  aria-label={t("specialist.temperature")}
                  className="w-28 rounded border border-line bg-graphite px-2 py-1 font-mono text-[11px] text-ink placeholder:text-muted/60"
                />
              </label>
            </div>
            <p className="mt-1 text-[10px] text-muted">{t("specialist.limitsHelp")}</p>
          </div>

          {isEditing ? (
            <p className="border-t border-line pt-3 text-[10px] text-muted">{t("specialist.editNote")}</p>
          ) : null}

          <div className="flex items-center gap-2 border-t border-line pt-3">
            {!isEditing && (
              <>
                <input
                  value={memberDisplayName}
                  onChange={(e) => setMemberDisplayName(e.target.value)}
                  placeholder={
                    name.trim()
                      ? t("specialist.memberNameDefault", { name: name.trim() })
                      : t("specialist.memberName")
                  }
                  className="flex-1 rounded border border-line bg-graphite px-2 py-1 text-xs text-ink placeholder:text-muted/60"
                />
                <select
                  value={teamId}
                  onChange={(e) => setTeamId(e.target.value)}
                  className="rounded border border-line bg-graphite px-2 py-1 text-xs text-ink"
                >
                  <option value="">{t("specialist.noTeam")}</option>
                  {teams.map((option) => (
                    <option key={option.id} value={option.id}>
                      {option.name}
                    </option>
                  ))}
                </select>
              </>
            )}
            {isEditing && (
              <button
                type="button"
                onClick={() => {
                  setPanelOpen(false);
                  onCancelEdit?.();
                }}
                className="rounded border border-line px-3 py-1 text-xs text-muted transition hover:text-ink"
              >
                {t("common.cancel")}
              </button>
            )}
            <button
              type="submit"
              disabled={busy || !name.trim() || !capabilities.trim()}
              className="ml-auto shrink-0 rounded bg-amber/90 px-3 py-1 text-xs font-medium text-onaccent transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
            >
              {busy
                ? t("specialist.saving")
                : isEditing
                  ? t("specialist.saveChanges")
                  : t("specialist.create")}
            </button>
          </div>
          {error && <p className="text-[11px] text-rose">{error}</p>}
        </form>
      )}
    </div>
  );
}
