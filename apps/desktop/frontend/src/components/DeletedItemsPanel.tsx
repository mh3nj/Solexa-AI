import { useState } from "react";
import { restoreMember, restoreTeam } from "../lib/api";
import type { MemberDto, TeamDto } from "../lib/types";
import { useI18n } from "../i18n";

// Round 27 built reversible soft delete and deliberately left deleted
// Members/Teams inline (dimmed, with their own Restore link) rather
// than moving them to a separate view, consistent with this rail's
// existing "show every member, don't hide anyone" philosophy. That
// works fine as long as you already know which team a deleted item
// used to be on. It stops working once a project has enough deleted
// history that finding one specific deleted thing means re-checking
// every team in the switcher one at a time, round 27's own README
// named this as the next likely gap.
//
// This panel doesn't replace the inline treatment (still the fastest
// way to notice something *you're looking at* is deleted); it adds
// the other direction: one place that lists everything deleted across
// the whole project, regardless of which team it's on or was on.
export function DeletedItemsPanel({
  members,
  teams,
  onChanged,
}: {
  members: MemberDto[];
  teams: TeamDto[];
  onChanged: () => void;
}) {
  const { t, locale } = useI18n();
  const [busyId, setBusyId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const deletedTeams = teams.filter((t) => t.deleted_at);
  const deletedMembers = members.filter((m) => m.deleted_at);
  const totalCount = deletedTeams.length + deletedMembers.length;

  function formatDeletedAt(iso: string): string {
    return new Date(iso).toLocaleString(locale);
  }

  async function handleRestoreTeam(team: TeamDto) {
    setBusyId(team.id);
    setError(null);
    try {
      await restoreTeam(team.id);
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  async function handleRestoreMember(member: MemberDto) {
    setBusyId(member.id);
    setError(null);
    try {
      await restoreMember(member.id);
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusyId(null);
    }
  }

  return (
    <div className="rounded-lg border border-line bg-panel p-4">
      <p className="mb-1 text-xs font-mono uppercase tracking-wide text-muted">
        {t("deleted.titleCount", { count: totalCount })}
      </p>
      <p className="mb-3 text-[11px] text-muted">{t("deleted.help")}</p>

      {totalCount === 0 ? (
        <p className="text-[11px] text-muted">{t("deleted.empty")}</p>
      ) : (
        <div className="space-y-3">
          {deletedTeams.length > 0 && (
            <div>
              <p className="mb-1.5 text-[10px] font-mono uppercase tracking-wide text-muted">
                {t("deleted.teams", { count: deletedTeams.length })}
              </p>
              <ul className="space-y-1.5">
                {deletedTeams.map((team) => (
                  <li
                    key={team.id}
                    className="flex items-center justify-between gap-2 rounded border border-line bg-graphite/60 px-2.5 py-1.5"
                  >
                    <div className="min-w-0">
                      <p className="truncate text-[12px] text-muted line-through">{team.name}</p>
                      <p className="truncate text-[10px] text-muted">
                        {t("deleted.deletedAt", { date: formatDeletedAt(team.deleted_at!) })}
                      </p>
                    </div>
                    <button
                      type="button"
                      disabled={busyId === team.id}
                      onClick={() => handleRestoreTeam(team)}
                      className="shrink-0 text-[10px] text-teal underline decoration-dotted transition hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
                    >
                      {t("common.restore")}
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          )}

          {deletedMembers.length > 0 && (
            <div>
              <p className="mb-1.5 text-[10px] font-mono uppercase tracking-wide text-muted">
                {t("deleted.members", { count: deletedMembers.length })}
              </p>
              <ul className="space-y-1.5">
                {deletedMembers.map((member) => {
                  // Round 28 confirmed this deliberately: deleting a
                  // member never touches any team's roster, so a
                  // deleted member can still be listed on one, surface
                  // that here rather than let it be a silent surprise.
                  const stillListedOn = teams.filter((t) => t.member_ids.includes(member.id)).map((t) => t.name);
                  return (
                    <li
                      key={member.id}
                      className="flex items-center justify-between gap-2 rounded border border-line bg-graphite/60 px-2.5 py-1.5"
                    >
                      <div className="min-w-0">
                        <p className="truncate text-[12px] text-muted line-through">{member.display_name}</p>
                        <p className="truncate text-[10px] text-muted">
                          {t("deleted.deletedAt", { date: formatDeletedAt(member.deleted_at!) })}
                          {stillListedOn.length > 0 &&
                            ` · ${t("deleted.stillOnRoster", { names: stillListedOn.join(", ") })}`}
                        </p>
                      </div>
                      <button
                        type="button"
                        disabled={busyId === member.id}
                        onClick={() => handleRestoreMember(member)}
                        className="shrink-0 text-[10px] text-teal underline decoration-dotted transition hover:text-ink disabled:cursor-not-allowed disabled:opacity-40"
                      >
                        {t("common.restore")}
                      </button>
                    </li>
                  );
                })}
              </ul>
            </div>
          )}
        </div>
      )}

      {error && <p className="mt-2 text-[11px] text-rose">{error}</p>}
    </div>
  );
}
