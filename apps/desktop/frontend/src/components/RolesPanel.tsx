import { useEffect, useState } from "react";
import {
  assignRole,
  assignRoleToSpecialist,
  createRole,
  getEnforceApproverRoles,
  listRoles,
  setEnforceApproverRoles,
  unassignRole,
  unassignRoleFromSpecialist,
} from "../lib/api";
import type { MemberDto, PermissionName, RoleDto, SpecialistDto } from "../lib/types";
import { useI18n } from "../i18n";

const ALL_PERMISSIONS: PermissionName[] = ["read", "propose", "edit", "review", "approve", "execute"];

// A role is a named set of permissions a member or specialist can hold.
// Holding Approve is what lets someone decide incoming changes in the
// team sync review queue. Roles live on this device only.
export function RolesPanel({
  members,
  specialists,
  onChanged,
}: {
  members: MemberDto[];
  specialists: SpecialistDto[];
  onChanged: () => void;
}) {
  const { t } = useI18n();
  const [roles, setRoles] = useState<RoleDto[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [newName, setNewName] = useState("");
  const [newPermissions, setNewPermissions] = useState<PermissionName[]>(["approve"]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [enforceApprover, setEnforceApprover] = useState(false);

  async function handleToggleEnforce() {
    setBusy(true);
    setError(null);
    try {
      await setEnforceApproverRoles(!enforceApprover);
      // Re-read instead of assuming: the engine decides.
      setEnforceApprover(await getEnforceApproverRoles());
    } catch (err) {
      // For example enabling it while nobody holds Approve.
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function refresh() {
    try {
      setRoles(await listRoles());
      setEnforceApprover(await getEnforceApproverRoles());
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

  function toggleNewPermission(p: PermissionName) {
    setNewPermissions((prev) => (prev.includes(p) ? prev.filter((x) => x !== p) : [...prev, p]));
  }

  async function handleCreate() {
    if (!newName.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await createRole(newName.trim(), newPermissions);
      setNewName("");
      setNewPermissions(["approve"]);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleToggleMember(roleId: string, member: MemberDto) {
    setBusy(true);
    setError(null);
    try {
      if (member.role_ids.includes(roleId)) {
        await unassignRole(member.id, roleId);
      } else {
        await assignRole(member.id, roleId);
      }
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleToggleSpecialist(roleId: string, specialist: SpecialistDto) {
    setBusy(true);
    setError(null);
    try {
      if ((specialist.role_ids ?? []).includes(roleId)) {
        await unassignRoleFromSpecialist(specialist.id, roleId);
      } else {
        await assignRoleToSpecialist(specialist.id, roleId);
      }
      onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  if (!loaded) return null;

  return (
    <div className="rounded-lg border border-line bg-panel p-4">
      <p className="mb-1 text-xs font-mono uppercase tracking-wide text-muted">{t("roles.title")}</p>
      <p className="mb-2 text-[11px] text-muted">{t("roles.help")}</p>
      <p className="mb-2 text-[11px] text-muted">{t("roles.deviceNote")}</p>
      <label className="mb-3 flex items-start gap-2 text-[11px] text-muted">
        <input
          type="checkbox"
          checked={enforceApprover}
          onChange={handleToggleEnforce}
          disabled={busy}
          aria-label={t("roles.requireApprove")}
          className="mt-0.5 accent-amber"
        />
        <span>
          {t("roles.requireApproveDetail", {
            permission: `${t("permission.approve")} (approve)`,
          })}
        </span>
      </label>

      <div className="mb-3 flex flex-wrap items-end gap-2">
        <input
          value={newName}
          onChange={(e) => setNewName(e.target.value)}
          placeholder={t("roles.namePlaceholder")}
          className="rounded border border-line bg-graphite px-2 py-1 text-[11px] text-ink placeholder:text-muted/60"
        />
        <div className="flex flex-wrap gap-2">
          {ALL_PERMISSIONS.map((p) => (
            <label key={p} className="flex items-center gap-1 text-[11px] text-muted">
              <input
                type="checkbox"
                checked={newPermissions.includes(p)}
                onChange={() => toggleNewPermission(p)}
                className="accent-amber"
              />
              {t(`permission.${p}`)}
            </label>
          ))}
        </div>
        <button
          onClick={handleCreate}
          disabled={busy || !newName.trim()}
          className="shrink-0 rounded bg-amber/90 px-3 py-1 text-[11px] font-medium text-graphite transition hover:bg-amber disabled:cursor-not-allowed disabled:opacity-40"
        >
          {t("roles.create")}
        </button>
      </div>

      {roles.length === 0 ? (
        <p className="text-[11px] text-muted">{t("roles.empty")}</p>
      ) : (
        <div className="space-y-2">
          {roles.map((role) => (
            <div key={role.id} className="rounded border border-line/60 p-2">
              <p className="text-[11px] font-medium text-ink">
                {role.name}{" "}
                <span className="text-muted">
                  {role.permissions.length > 0
                    ? role.permissions.map((p) => t(`permission.${p}`)).join(", ")
                    : t("roles.noPermissions")}
                </span>
              </p>
              <div className="mt-1 flex flex-wrap gap-2">
                {members.map((m) => (
                  <label key={m.id} className="flex items-center gap-1 text-[11px] text-muted">
                    <input
                      type="checkbox"
                      checked={m.role_ids.includes(role.id)}
                      onChange={() => handleToggleMember(role.id, m)}
                      disabled={busy}
                      className="accent-teal"
                    />
                    {m.display_name}
                  </label>
                ))}
                {specialists.map((s) => (
                  <label key={s.id} className="flex items-center gap-1 text-[11px] text-muted">
                    <input
                      type="checkbox"
                      checked={(s.role_ids ?? []).includes(role.id)}
                      onChange={() => handleToggleSpecialist(role.id, s)}
                      disabled={busy}
                      className="accent-sky"
                    />
                    {s.name} <span className="text-muted/70">{t("roles.specialistTag")}</span>
                  </label>
                ))}
              </div>
            </div>
          ))}
        </div>
      )}

      {error && <p className="mt-2 text-[11px] text-rose">{error}</p>}
    </div>
  );
}
