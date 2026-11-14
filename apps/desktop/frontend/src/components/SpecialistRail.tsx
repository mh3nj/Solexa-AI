import { useState } from "react";
import { ProjectSwitcher } from "./ProjectSwitcher";
import {
  addMemberToTeam,
  createTeam,
  deleteMember,
  deleteTeam,
  removeMemberFromTeam,
  restoreMember,
  restoreTeam,
  setTeamParent,
  updateMember,
  updateTeam,
} from "../lib/api";
import type { MemberDto, SpecialistDto, TeamDto } from "../lib/types";
import { useI18n } from "../i18n";
import { Logo } from "./icons";

function initials(name: string): string {
  return name
    .split(" ")
    .map((w) => w[0])
    .join("")
    .slice(0, 2)
    .toUpperCase();
}

// The left rail shows the Team as Members (Phase 6), not raw
// Specialists, a human is a member too, and an AI member's
// capabilities/benchmark are shown as *its* attributes, resolved via
// `specialist_ids` rather than the rail talking to Specialists
// directly. This is the same shift the engine made: Member is the
// accountable participant, Specialist is the capability config an AI
// member runs (spec: "MODEL != MEMBER").
//
// Team rename and member rename are handled inline, right here, rather
// than a separate form component, unlike specialist creation/editing,
// which is structured enough (models, permissions, escalation rules)
// to earn its own form, a rename is just one or two text fields with
// nowhere else it needs to live.
// Round 54: a specialist's own generation limits (round 52), shown on its
// card so they're visible without opening the editor. `null`/`undefined`
// means unset and shows nothing; a temperature of 0 is a real setting
// (greedy) and must show, a truthiness check would silently hide it.
function formatGenerationLimits(
  specialist: { max_tokens?: number | null; temperature?: number | null },
  t: (key: string, vars?: Record<string, string | number>) => string,
): string | null {
  const parts: string[] = [];
  if (specialist.max_tokens != null && specialist.max_tokens > 0) {
    parts.push(t("specialist.limitTokens", { count: specialist.max_tokens }));
  }
  if (specialist.temperature != null) {
    parts.push(t("specialist.limitTemperature", { value: specialist.temperature }));
  }
  return parts.length > 0 ? parts.join(" · ") : null;
}

export function SpecialistRail({
  team,
  teams,
  members,
  specialists,
  onEditSpecialist,
  onToggleSpecialistActive,
  onExportSpecialist,
  onExportTeam,
  onSaveSpecialistToRegistry,
  onSaveTeamToRegistry,
  onChanged,
  onSelectTeam,
  onProjectSwitched,
}: {
  team: TeamDto | null;
  teams: TeamDto[];
  members: MemberDto[];
  specialists: SpecialistDto[];
  onEditSpecialist?: (specialist: SpecialistDto) => void;
  onToggleSpecialistActive?: (specialist: SpecialistDto) => void;
  onExportSpecialist?: (specialist: SpecialistDto) => void;
  onExportTeam?: (team: TeamDto) => void;
  onSaveSpecialistToRegistry?: (specialist: SpecialistDto) => void;
  onSaveTeamToRegistry?: (team: TeamDto) => void;
  onChanged?: () => void;
  onSelectTeam?: (teamId: string) => void;
  /** Round 35: reload everything, the whole view belongs to the old project. */
  onProjectSwitched: () => void;
}) {
  const { t, locale } = useI18n();
  const specialistById = new Map(specialists.map((s) => [s.id, s]));

  const [editingTeam, setEditingTeam] = useState(false);
  const [teamNameDraft, setTeamNameDraft] = useState("");
  const [teamMissionDraft, setTeamMissionDraft] = useState("");
  const [teamParentDraft, setTeamParentDraft] = useState("");
  const [creatingTeam, setCreatingTeam] = useState(false);
  const [newTeamName, setNewTeamName] = useState("");
  const [newTeamMission, setNewTeamMission] = useState("");
  const [newTeamParentId, setNewTeamParentId] = useState("");
  const [editingMemberId, setEditingMemberId] = useState<string | null>(null);
  const [memberNameDraft, setMemberNameDraft] = useState("");
  const [memberDescDraft, setMemberDescDraft] = useState("");
  const [error, setError] = useState<string | null>(null);

  function startEditingTeam() {
    if (!team) return;
    setTeamNameDraft(team.name);
    setTeamMissionDraft(team.mission);
    setTeamParentDraft(team.parent_team_id ?? "");
    setError(null);
    setEditingTeam(true);
  }

  async function saveTeam() {
    if (!team) return;
    try {
      await updateTeam({ teamId: team.id, name: teamNameDraft, mission: teamMissionDraft });
      if (teamParentDraft !== (team.parent_team_id ?? "")) {
        await setTeamParent(team.id, teamParentDraft || undefined);
      }
      setEditingTeam(false);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function submitNewTeam() {
    if (!newTeamName.trim()) {
      setError(t("team.nameRequired"));
      return;
    }
    try {
      const result = await createTeam({
        name: newTeamName.trim(),
        mission: newTeamMission.trim(),
        parentTeamId: newTeamParentId || undefined,
      });
      setNewTeamName("");
      setNewTeamMission("");
      setNewTeamParentId("");
      setCreatingTeam(false);
      setError(null);
      onChanged?.();
      onSelectTeam?.(result.team_id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  function startEditingMember(member: MemberDto) {
    setMemberNameDraft(member.display_name);
    setMemberDescDraft(member.description);
    setError(null);
    setEditingMemberId(member.id);
  }

  async function saveMember(member: MemberDto) {
    try {
      await updateMember({
        memberId: member.id,
        displayName: memberNameDraft,
        description: memberDescDraft,
        specialistIds: member.specialist_ids,
      });
      setEditingMemberId(null);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function removeMember(member: MemberDto) {
    if (!team) return;
    try {
      await removeMemberFromTeam(team.id, member.id);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function addMember(member: MemberDto) {
    if (!team) return;
    try {
      await addMemberToTeam(team.id, member.id);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  // ---- soft delete (round 27), never a hard delete, always
  // reversible via the matching restore call. window.confirm is the
  // simplest honest confirmation available without a dedicated dialog
  // component; deleting is reversible, so a lightweight confirm fits.

  async function handleDeleteMember(member: MemberDto) {
    if (!window.confirm(t("member.confirmDelete", { name: member.display_name }))) return;
    try {
      await deleteMember(member.id);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleRestoreMember(member: MemberDto) {
    try {
      await restoreMember(member.id);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleDeleteTeam(selected: TeamDto) {
    if (!window.confirm(t("team.confirmDelete", { name: selected.name }))) return;
    try {
      await deleteTeam(selected.id);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleRestoreTeam(selected: TeamDto) {
    try {
      await restoreTeam(selected.id);
      setError(null);
      onChanged?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  function formatDeletedAt(iso: string): string {
    return new Date(iso).toLocaleString(locale);
  }

  return (
    <aside className="flex w-64 shrink-0 flex-col border-r border-line bg-panel/60 px-4 py-6">
      <div className="mb-6 flex items-center gap-2">
        <Logo className="h-5 w-5 text-amber" />
        <div>
          <p className="text-sm font-semibold tracking-tight text-ink">SOLEXA</p>
          <p className="text-[11px] text-muted">{t("app.tagline")}</p>
        </div>
      </div>

      <ProjectSwitcher onSwitched={onProjectSwitched} />

      {teams.length > 1 && (
        <select
          value={team?.id ?? ""}
          onChange={(e) => onSelectTeam?.(e.target.value)}
          className="mb-2 w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink"
        >
          {teams.map((option) => (
            <option key={option.id} value={option.id}>
              {option.name}
              {option.deleted_at ? ` ${t("team.deletedTag")}` : ""}
            </option>
          ))}
        </select>
      )}

      {editingTeam && team ? (
        <div className="mb-2 space-y-1.5 rounded border border-line bg-graphite/60 p-2">
          <input
            value={teamNameDraft}
            onChange={(e) => setTeamNameDraft(e.target.value)}
            placeholder={t("team.name")}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink"
          />
          <input
            value={teamMissionDraft}
            onChange={(e) => setTeamMissionDraft(e.target.value)}
            placeholder={t("team.mission")}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
          />
          <select
            value={teamParentDraft}
            onChange={(e) => setTeamParentDraft(e.target.value)}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
          >
            <option value="">{t("team.noParent")}</option>
            {teams
              .filter((option) => option.id !== team.id)
              .map((option) => (
                <option key={option.id} value={option.id}>
                  {t("team.under", { name: option.name })}
                </option>
              ))}
          </select>
          <div className="flex gap-2">
            <button
              type="button"
              onClick={saveTeam}
              className="rounded bg-amber/90 px-2 py-0.5 text-[11px] font-medium text-onaccent transition hover:bg-amber"
            >
              {t("common.save")}
            </button>
            <button
              type="button"
              onClick={() => setEditingTeam(false)}
              className="rounded border border-line px-2 py-0.5 text-[11px] text-muted transition hover:text-ink"
            >
              {t("common.cancel")}
            </button>
          </div>
        </div>
      ) : (
        <div className="mb-2 flex items-center justify-between">
          <p className="text-xs font-mono uppercase tracking-wide text-muted">
            {team?.name ?? t("team.name")} ({team?.member_ids.length ?? members.length})
            {team?.deleted_at && (
              <span className="ml-1 text-rose">
                · {t("deleted.deletedAt", { date: formatDeletedAt(team.deleted_at) })}
              </span>
            )}
          </p>
          {team && (
            <div className="flex gap-2">
              {team.deleted_at ? (
                <button
                  type="button"
                  onClick={() => handleRestoreTeam(team)}
                  className="text-[10px] text-teal underline decoration-dotted transition hover:text-ink"
                >
                  {t("common.restore")}
                </button>
              ) : (
                <>
                  <button
                    type="button"
                    onClick={startEditingTeam}
                    className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                  >
                    {t("common.edit")}
                  </button>
                  {onExportTeam && (
                    <button
                      type="button"
                      onClick={() => onExportTeam(team)}
                      className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                    >
                      {t("common.export")}
                    </button>
                  )}
                  {onSaveTeamToRegistry && (
                    <button
                      type="button"
                      onClick={() => onSaveTeamToRegistry(team)}
                      className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                    >
                      {t("specialist.saveToRegistry")}
                    </button>
                  )}
                  <button
                    type="button"
                    onClick={() => handleDeleteTeam(team)}
                    className="text-[10px] text-muted underline decoration-dotted transition hover:text-rose"
                  >
                    {t("common.delete")}
                  </button>
                </>
              )}
            </div>
          )}
        </div>
      )}

      {creatingTeam ? (
        <div className="mb-4 space-y-1.5 rounded border border-line bg-graphite/60 p-2">
          <input
            value={newTeamName}
            onChange={(e) => setNewTeamName(e.target.value)}
            placeholder={t("team.namePlaceholder")}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink"
          />
          <input
            value={newTeamMission}
            onChange={(e) => setNewTeamMission(e.target.value)}
            placeholder={t("team.missionPlaceholder")}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
          />
          <select
            value={newTeamParentId}
            onChange={(e) => setNewTeamParentId(e.target.value)}
            className="w-full rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
          >
            <option value="">{t("team.noParent")}</option>
            {teams.map((option) => (
              <option key={option.id} value={option.id}>
                {t("team.under", { name: option.name })}
              </option>
            ))}
          </select>
          <div className="flex gap-2">
            <button
              type="button"
              onClick={submitNewTeam}
              className="rounded bg-amber/90 px-2 py-0.5 text-[11px] font-medium text-onaccent transition hover:bg-amber"
            >
              {t("common.create")}
            </button>
            <button
              type="button"
              onClick={() => setCreatingTeam(false)}
              className="rounded border border-line px-2 py-0.5 text-[11px] text-muted transition hover:text-ink"
            >
              {t("common.cancel")}
            </button>
          </div>
        </div>
      ) : (
        <button
          type="button"
          onClick={() => setCreatingTeam(true)}
          className="mb-4 text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
        >
          {t("team.create")}
        </button>
      )}

      {/* Deliberately renders every project member, not just this
          team's roster, there's no other view yet where a member
          removed from this team would still be visible, and hiding
          them here entirely would look like they'd been deleted
          rather than just taken off one team's list. The header count
          above reflects actual team membership; this list intentionally
          doesn't filter to match it. A soft-deleted member (round 27)
          also stays visible here, dimmed with a "Restore" link, rather
          than disappearing into a separate view, for the same reason. */}
      <ul className="space-y-2">
        {members.map((member) => {
          const isHuman = member.member_type === "human";
          const linkedSpecialists = member.specialist_ids
            .map((id) => specialistById.get(id))
            .filter((s): s is SpecialistDto => Boolean(s));
          const isEditingThisMember = editingMemberId === member.id;

          return (
            <li key={member.id} className="rounded-lg border border-line bg-graphite/60 p-3">
              {isEditingThisMember ? (
                <div className="space-y-1.5">
                  <input
                    value={memberNameDraft}
                    onChange={(e) => setMemberNameDraft(e.target.value)}
                    placeholder={t("member.displayName")}
                    className="w-full rounded border border-line bg-graphite px-2 py-1 text-xs text-ink"
                  />
                  {!isHuman && (
                    <input
                      value={memberDescDraft}
                      onChange={(e) => setMemberDescDraft(e.target.value)}
                      placeholder={t("member.description")}
                      className="w-full rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink"
                    />
                  )}
                  <div className="flex gap-2">
                    <button
                      type="button"
                      onClick={() => saveMember(member)}
                      className="rounded bg-amber/90 px-2 py-0.5 text-[11px] font-medium text-onaccent transition hover:bg-amber"
                    >
                      {t("common.save")}
                    </button>
                    <button
                      type="button"
                      onClick={() => setEditingMemberId(null)}
                      className="rounded border border-line px-2 py-0.5 text-[11px] text-muted transition hover:text-ink"
                    >
                      {t("common.cancel")}
                    </button>
                  </div>
                </div>
              ) : (
                <div>
                  <div className="flex items-center gap-2.5">
                    <span
                      className={`flex h-8 w-8 shrink-0 items-center justify-center rounded-full font-mono text-xs font-semibold ${
                        member.deleted_at ? "bg-line/40 text-muted" : isHuman ? "bg-teal/15 text-teal" : "bg-amber/15 text-amber"
                      }`}
                    >
                      {isHuman ? "H" : initials(member.display_name)}
                    </span>
                    <div className="min-w-0 flex-1">
                      <p className={`truncate text-sm ${member.deleted_at ? "text-muted line-through" : "text-ink"}`}>
                        {member.display_name}
                      </p>
                      <p className="truncate text-[11px] text-muted">
                        {member.deleted_at
                          ? t("deleted.deletedAt", { date: formatDeletedAt(member.deleted_at) })
                          : isHuman
                            ? t("member.human")
                            : member.description}
                      </p>
                    </div>
                  </div>
                  <div className="mt-1.5 flex gap-2">
                    {member.deleted_at ? (
                      <button
                        type="button"
                        onClick={() => handleRestoreMember(member)}
                        className="text-[10px] text-teal underline decoration-dotted transition hover:text-ink"
                      >
                        {t("common.restore")}
                      </button>
                    ) : (
                      <>
                        <button
                          type="button"
                          onClick={() => startEditingMember(member)}
                          className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                        >
                          {t("common.edit")}
                        </button>
                        {team && (
                          team.member_ids.includes(member.id) ? (
                            <button
                              type="button"
                              onClick={() => removeMember(member)}
                              className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                            >
                              {t("specialist.removeFromTeam")}
                            </button>
                          ) : (
                            <button
                              type="button"
                              onClick={() => addMember(member)}
                              className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                            >
                              {t("specialist.addToTeamAction")}
                            </button>
                          )
                        )}
                        <button
                          type="button"
                          onClick={() => handleDeleteMember(member)}
                          className="text-[10px] text-muted underline decoration-dotted transition hover:text-rose"
                        >
                          {t("common.delete")}
                        </button>
                      </>
                    )}
                  </div>
                </div>
              )}

              {linkedSpecialists.map((specialist) => {
                const isArchived = specialist.active === false;
                return (
                  <div key={specialist.id} className="mt-2 border-t border-line pt-2">
                    <div className="flex flex-wrap items-center gap-1">
                      {isArchived && (
                        <span className="rounded bg-rose/15 px-1.5 py-0.5 font-mono text-[10px] text-rose">
                          {t("specialist.archived")}
                        </span>
                      )}
                      {specialist.capabilities.map((cap) => (
                        <span key={cap} className="rounded bg-line/50 px-1.5 py-0.5 font-mono text-[10px] text-muted">
                          {cap}
                        </span>
                      ))}
                    </div>
                    {specialist.benchmark && (
                      <p className="mt-1 font-mono text-[10px] text-muted">
                        {specialist.benchmark.verification_passed_count + specialist.benchmark.verification_failed_count === 0
                          ? t("specialist.noTrackRecord")
                          : t("specialist.trackRecord", {
                              rate: Math.round(specialist.benchmark.success_rate * 100),
                              passed: specialist.benchmark.verification_passed_count,
                              total:
                                specialist.benchmark.verification_passed_count +
                                specialist.benchmark.verification_failed_count,
                            })}
                      </p>
                    )}
                    {formatGenerationLimits(specialist, t) && (
                      <p
                        className="mt-1 font-mono text-[10px] text-muted"
                        aria-label={t("specialist.generationLimits")}
                      >
                        {formatGenerationLimits(specialist, t)}
                      </p>
                    )}
                    {(onEditSpecialist || onToggleSpecialistActive || onExportSpecialist || onSaveSpecialistToRegistry) && (
                      <div className="mt-1.5 flex gap-2">
                        {onEditSpecialist && (
                          <button
                            type="button"
                            onClick={() => onEditSpecialist(specialist)}
                            className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                          >
                            {t("common.edit")}
                          </button>
                        )}
                        {onToggleSpecialistActive && (
                          <button
                            type="button"
                            onClick={() => onToggleSpecialistActive(specialist)}
                            className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                          >
                            {isArchived ? t("specialist.reactivate") : t("specialist.archive")}
                          </button>
                        )}
                        {onExportSpecialist && (
                          <button
                            type="button"
                            onClick={() => onExportSpecialist(specialist)}
                            className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                          >
                            {t("common.export")}
                          </button>
                        )}
                        {onSaveSpecialistToRegistry && (
                          <button
                            type="button"
                            onClick={() => onSaveSpecialistToRegistry(specialist)}
                            className="text-[10px] text-muted underline decoration-dotted transition hover:text-ink"
                          >
                            {t("specialist.saveToRegistry")}
                          </button>
                        )}
                      </div>
                    )}
                  </div>
                );
              })}
            </li>
          );
        })}
      </ul>

      {error && <p className="mt-2 text-[11px] text-rose">{error}</p>}

      <div className="mt-auto pt-6">
        <p className="text-[11px] leading-relaxed text-muted">{t("app.localFirst")}</p>
      </div>
    </aside>
  );
}
