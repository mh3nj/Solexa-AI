/**
 * The frontend's entire connection to the engine. Every exported
 * function here is a thin call to a Rust Tauri command, there is no
 * orchestration logic in this file or anywhere else in the frontend
 * (spec rule: "the frontend sends commands and consumes state/events;
 * do not put orchestration logic inside React").
 *
 * MOCK MODE: when this app isn't actually running inside the Tauri
 * shell (e.g. `npm run dev` opened in a plain browser, for fast UI
 * iteration), `invoke` falls back to a small in-memory simulation that
 * reproduces the same event sequence the real Coordinator emits. It
 * exists purely so the UI is legible without a compiled Tauri binary,
 * it is clearly isolated below and never used when `__TAURI_INTERNALS__`
 * is present.
 */

import type {
  ConflictDto,
  CreateSpecialistInput,
  CreateSpecialistResult,
  CreateTeamInput,
  CreateTeamResult,
  DecisionDto,
  EventDto,
  GitPullResultDto,
  GitPushResultDto,
  GitSyncStatusDto,
  ImportSpecialistPackageResult,
  ImportTeamPackageResult,
  InstallFromRegistryResult,
  KnowledgeItemDto,
  MemberDto,
  ModelDto,
  ModelWithStatusDto,
  PendingSyncChangeDto,
  PermissionName,
  RegistryEntryDto,
  ResourceSnapshotDto,
  RoleDto,
  SaveToRegistryResult,
  SpecialistDto,
  SubmissionDto,
  TaskDto,
  TaskState,
  TeamDto,
  UpdateMemberInput,
  UpdateSpecialistInput,
  UpdateTeamInput,
  ProjectDto,
} from "./types";

/** The shape of a `Specialist` as it appears nested inside a parsed
 * `SpecialistPackage` or `TeamPackage` JSON document, used by both
 * `importSpecialistPackage` and `importTeamPackage`'s validation. */
interface SpecialistPackageSpecialistLike {
  name?: string;
  role?: string;
  domain?: string;
  capabilities?: string[];
  preferred_model_ids?: string[];
  tools?: string[];
  knowledge_scope?: string[];
  permissions?: PermissionName[];
  output_schema?: unknown;
  escalation_policy?: {
    escalate_on_weak_confidence?: boolean;
    escalate_on_verification_failure?: boolean;
    requires_model_review?: boolean;
    requires_specialist_review?: boolean;
  };
  // Round 52: optional generation limits; absent in packages exported
  // before this round, which import as unset.
  max_tokens?: number | null;
  temperature?: number | null;
}

const isTauri =
  typeof window !== "undefined" &&
  ("__TAURI_INTERNALS__" in window || "__TAURI__" in window);

export async function submitRequest(text: string, priority?: number): Promise<SubmissionDto> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("submit_request", { text, priority: priority ?? null });
  }
  return mock.submitRequest(text, priority);
}

// ---- app settings, startup, tray (round 67) -------------------------
//
// SQLite is the source of truth for these. The settings provider keeps a
// localStorage copy purely so the first frame paints the right theme and
// language without waiting for the IPC round trip.

export async function getAppSettings(): Promise<Record<string, string>> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("get_app_settings");
  }
  return {};
}

export async function setAppSetting(key: string, value: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_app_setting", { key, value });
  }
}

export async function setStartupMode(mode: "off" | "app" | "tray"): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_startup_mode", { mode });
    return;
  }
  window.localStorage.setItem("solexa.mock.startup", mode);
}

export async function getStartupEnabled(): Promise<boolean> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("startup_enabled");
  }
  return (window.localStorage.getItem("solexa.mock.startup") ?? "off") !== "off";
}

export async function setTrayMenuLanguage(language: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("update_tray_menu", { language });
  }
}

export async function listTasks(): Promise<TaskDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_tasks");
  }
  return mock.listTasks();
}

export async function getTrace(correlationId: string): Promise<EventDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("get_trace", { correlationId });
  }
  return mock.getTrace(correlationId);
}

export async function listSpecialists(): Promise<SpecialistDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_specialists");
  }
  return mock.listSpecialists();
}

export async function listMembers(): Promise<MemberDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_members");
  }
  return mock.listMembers();
}

/** Renames a member and/or, for an AI member, swaps which
 * specialist(s) it runs (spec section 11, "Model Swapping"). Never
 * touches `member_type`, that's identity, not something an edit
 * changes. */
export async function updateMember(input: UpdateMemberInput): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("update_member", {
      memberId: input.memberId,
      displayName: input.displayName,
      description: input.description,
      specialistIds: input.specialistIds,
    });
    return;
  }
  mock.updateMember(input);
}

export async function listModels(): Promise<ModelDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_models");
  }
  return mock.listModels();
}

// Round 41: per-project model configuration.
export async function listModelsWithStatus(): Promise<ModelWithStatusDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_models_with_status");
  }
  return mock.listModelsWithStatus();
}

export async function setModelEnabled(modelId: string, enabled: boolean): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_model_enabled", { modelId, enabled });
    return;
  }
  mock.setModelEnabled(modelId, enabled);
}

// Adopted from a parallel build: the Resource Center.
export async function getResourceSnapshot(): Promise<ResourceSnapshotDto> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("get_resource_snapshot");
  }
  return mock.getResourceSnapshot();
}

/** Round 60: resizes the engine's resource budget while it is running (the
 * Resource Center's controls). Running work is never evicted; a shrink that
 * would strand an already-queued task is refused with a message. The real
 * engine remembers the new budget across restarts (round 61); this dev mock
 * only lives as long as the page. */
export async function setResourceBudget(vramMb: number, ramMb: number, maxConcurrent: number): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_resource_budget", { vramMb, ramMb, maxConcurrent });
    return;
  }
  mock.setResourceBudget(vramMb, ramMb, maxConcurrent);
}

/** Round 62: puts the resource budget back to the engine's built-in default and
 * forgets any saved override. Refused with a message if the default would
 * strand an already-queued task. */
export async function resetResourceBudget(): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("reset_resource_budget");
    return;
  }
  mock.resetResourceBudget();
}

/** The structured specialist-creation flow (spec section 13: name,
 * role, domain, capabilities, models, tools, knowledge scope,
 * permissions, output format, escalation rules, "not a giant
 * system-prompt textbox"). Also creates the AI member that runs the
 * new specialist and, if given, adds that member to a team, mirroring
 * what `seed.rs` does for every seeded specialist, so a specialist
 * created this way is immediately visible in the Team rail too, not
 * just routable. */
/** Round 54: mirrors `Specialist::validate_generation_limits` so the mock
 * refuses exactly what the real engine refuses (same messages). The form
 * checks first, but the backend no longer depends on that. */
function assertValidGenerationLimits(maxTokens?: number | null, temperature?: number | null): void {
  if (maxTokens === 0) {
    throw new Error("output limit must be at least 1 token (leave it unset for no limit)");
  }
  if (temperature != null && (!Number.isFinite(temperature) || temperature < 0 || temperature > 2)) {
    throw new Error(`temperature must be a number from 0 to 2 (got ${temperature})`);
  }
}

export async function createSpecialist(input: CreateSpecialistInput): Promise<CreateSpecialistResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("create_specialist", {
      name: input.name,
      role: input.role,
      domain: input.domain,
      capabilities: input.capabilities,
      preferredModelIds: input.preferredModelIds,
      tools: input.tools,
      knowledgeScope: input.knowledgeScope,
      permissions: input.permissions,
      outputSchema: input.outputSchema ?? null,
      escalateOnWeakConfidence: input.escalateOnWeakConfidence,
      escalateOnVerificationFailure: input.escalateOnVerificationFailure,
      requiresModelReview: input.requiresModelReview,
      requiresSpecialistReview: input.requiresSpecialistReview,
      maxTokens: input.maxTokens ?? null,
      temperature: input.temperature ?? null,
      memberDisplayName: input.memberDisplayName,
      addToTeamId: input.addToTeamId ?? null,
    });
  }
  return mock.createSpecialist(input);
}

/** Rewrites an existing specialist's configuration through the same
 * structured shape as `createSpecialist`. Doesn't touch the AI member
 * or team membership `createSpecialist` set up, and never touches
 * `active`, that's `setSpecialistActive`'s job, kept deliberately
 * separate so an unrelated field edit can never silently reactivate an
 * archived specialist. */
export async function updateSpecialist(input: UpdateSpecialistInput): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("update_specialist", {
      specialistId: input.specialistId,
      name: input.name,
      role: input.role,
      domain: input.domain,
      capabilities: input.capabilities,
      preferredModelIds: input.preferredModelIds,
      tools: input.tools,
      knowledgeScope: input.knowledgeScope,
      permissions: input.permissions,
      outputSchema: input.outputSchema ?? null,
      escalateOnWeakConfidence: input.escalateOnWeakConfidence,
      escalateOnVerificationFailure: input.escalateOnVerificationFailure,
      requiresModelReview: input.requiresModelReview,
      requiresSpecialistReview: input.requiresSpecialistReview,
      maxTokens: input.maxTokens ?? null,
      temperature: input.temperature ?? null,
    });
    return;
  }
  mock.updateSpecialist(input);
}

/** Archives (`active: false`) or reactivates (`active: true`) a
 * specialist without deleting it. An archived specialist stops being
 * routable and stops appearing in specialist-discovery rankings, but
 * keeps its full history and stays visible via `listSpecialists`. */
export async function setSpecialistActive(specialistId: string, active: boolean): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_specialist_active", { specialistId, active });
    return;
  }
  mock.setSpecialistActive(specialistId, active);
}

/** Serializes a specialist's full configuration and measured benchmark
 * into a portable, pretty-printed JSON document (spec Phase 7's
 * "specialist packaging"). Returns the JSON as a plain string, no
 * file-dialog plumbing here, callers decide what to do with the text. */
export async function exportSpecialistPackage(specialistId: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("export_specialist_package", { specialistId });
  }
  return mock.exportSpecialistPackage(specialistId);
}

/** The "installation" half, registers a new specialist from a
 * previously-exported package's JSON, with a fresh id and always
 * active regardless of what the package says. */
export async function importSpecialistPackage(packageJson: string): Promise<ImportSpecialistPackageResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("import_specialist_package", { packageJson });
  }
  return mock.importSpecialistPackage(packageJson);
}

/** Serializes a team's AI structure, name, mission, and every AI
 * member's full packaged specialist, into a portable JSON document.
 * Human members and hierarchy are excluded (see the domain type's
 * doc comment for why). */
export async function exportTeamPackage(teamId: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("export_team_package", { teamId });
  }
  return mock.exportTeamPackage(teamId);
}

/** Installs a new team, and every AI member/specialist it packaged,
 * from a previously-exported team package's JSON. */
export async function importTeamPackage(packageJson: string): Promise<ImportTeamPackageResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("import_team_package", { packageJson });
  }
  return mock.importTeamPackage(packageJson);
}

/** Saves a copy of a specialist into the local registry, a personal
 * library independent of any one project. */
export async function saveSpecialistToRegistry(specialistId: string): Promise<SaveToRegistryResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("save_specialist_to_registry", { specialistId });
  }
  return mock.saveSpecialistToRegistry(specialistId);
}

/** Saves a copy of a team's AI structure into the local registry. */
export async function saveTeamToRegistry(teamId: string): Promise<SaveToRegistryResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("save_team_to_registry", { teamId });
  }
  return mock.saveTeamToRegistry(teamId);
}

/** Every entry in the local registry, newest first, global, not
 * scoped to the currently open project. */
export async function listRegistryEntries(): Promise<RegistryEntryDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_registry_entries");
  }
  return mock.listRegistryEntries();
}

/** One registry entry's full package JSON, the same document
 * `installFromRegistry` installs from, displayable the same way a
 * live specialist's/team's export is. */
export async function getRegistryEntryPackageJson(entryId: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("get_registry_entry_package_json", { entryId });
  }
  return mock.getRegistryEntryPackageJson(entryId);
}

/** Installs a saved registry entry into the currently open project. */
export async function installFromRegistry(entryId: string): Promise<InstallFromRegistryResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("install_from_registry", { entryId });
  }
  return mock.installFromRegistry(entryId);
}

/** Permanently removes an entry from the local registry, a genuine
 * delete, not an archive. */
export async function removeFromRegistry(entryId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("remove_from_registry", { entryId });
    return;
  }
  mock.removeFromRegistry(entryId);
}

export async function listTeams(): Promise<TeamDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_teams");
  }
  return mock.listTeams();
}

/** Renames a team and/or updates its mission statement. Never touches
 * membership (use `addMemberToTeam`-equivalent flows, membership is
 * set via `createSpecialist`'s team pick, not here) or hierarchy. */
export async function updateTeam(input: UpdateTeamInput): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("update_team", { teamId: input.teamId, name: input.name, mission: input.mission });
    return;
  }
  mock.updateTeam(input);
}

/** Registers a new team, the first reachable way to create a second
 * one; until now the only team in the app was whatever seed data
 * provided. Also what makes `setTeamParent` meaningful: re-parenting
 * needs somewhere else to move a team to. */
export async function createTeam(input: CreateTeamInput): Promise<CreateTeamResult> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("create_team", { name: input.name, mission: input.mission, parentTeamId: input.parentTeamId ?? null });
  }
  return mock.createTeam(input);
}

/** Moves a team to a new place in the hierarchy, or detaches it to the
 * top level when `newParentId` is omitted. Kept as its own call,
 * separate from `updateTeam`, the engine treats a rename and a
 * structural move as different actions on purpose. */
export async function setTeamParent(teamId: string, newParentId: string | undefined): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_team_parent", { teamId, newParentId: newParentId ?? null });
    return;
  }
  mock.setTeamParent(teamId, newParentId);
}

/** The inverse of joining a team at specialist-creation time, removes
 * a member from a team's roster without deleting the member itself. */
/** Adds an existing, already-registered member to a team's roster,
 * the same operation `createSpecialist` triggers internally when a
 * team is picked, now reachable on its own. */
export async function addMemberToTeam(teamId: string, memberId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("add_member_to_team", { teamId, memberId });
    return;
  }
  mock.addMemberToTeam(teamId, memberId);
}

export async function removeMemberFromTeam(teamId: string, memberId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("remove_member_from_team", { teamId, memberId });
    return;
  }
  mock.removeMemberFromTeam(teamId, memberId);
}

// ---- soft delete (round 27) -------------------------------------------
//
// Never a hard delete, see solexa_domain::team's doc comment on
// `deleted_at`. A delete/restore is just another field change, so it
// syncs through the exact same mechanism any other edit does.

export async function deleteMember(memberId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("delete_member", { memberId });
    return;
  }
  mock.deleteMember(memberId);
}

export async function restoreMember(memberId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("restore_member", { memberId });
    return;
  }
  mock.restoreMember(memberId);
}

export async function deleteTeam(teamId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("delete_team", { teamId });
    return;
  }
  mock.deleteTeam(teamId);
}

export async function restoreTeam(teamId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("restore_team", { teamId });
    return;
  }
  mock.restoreTeam(teamId);
}

/** Always empty in mock mode, see the note on `MockEngine` below: the
 * mock only simulates a single specialist, so a Phase 5 task conflict
 * (which needs two disagreeing specialists) can never occur there, and
 * a round-22 sync conflict needs a real, independently-diverged git
 * clone, which mock mode has no way to simulate either. */
export async function listConflicts(): Promise<ConflictDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_conflicts");
  }
  return [];
}

export async function resolveConflict(
  conflictId: string,
  chosenSpecialistId: string,
  approvingMemberId: string,
): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("resolve_conflict", {
      conflictId,
      chosenSpecialistId,
      approvingMemberId,
    });
    return;
  }
  throw new Error("Conflict resolution isn't available in mock mode, it needs the real engine.");
}

export async function resolveSyncConflict(
  conflictId: string,
  chosenSpecialistId: string,
  approvingMemberId: string,
): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("resolve_sync_conflict", {
      conflictId,
      chosenSpecialistId,
      approvingMemberId,
    });
    return;
  }
  throw new Error("Sync conflict resolution isn't available in mock mode, it needs the real engine.");
}

// ---- GitHub PR mirroring (round 30, Phase 8) -------------------------
//
// Neither of these is implemented in mock mode, for the same reason
// resolveSyncConflict above isn't: a sync conflict only exists after a
// real git pull between two real devices, which mock mode has never
// simulated. There's no conflict for these to act on in mock mode in
// the first place, let alone a real `gh` CLI to shell out to.

export async function sendConflictToGithub(conflictId: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<string>("send_conflict_to_github", { conflictId });
  }
  throw new Error("Sending a conflict to GitHub isn't available in mock mode, it needs the real engine and a real `gh` login.");
}

export async function checkConflictGithubPrStatus(conflictId: string, checkingMemberId: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<string>("check_conflict_github_pr_status", { conflictId, checkingMemberId });
  }
  throw new Error("Checking a GitHub PR's status isn't available in mock mode, it needs the real engine and a real `gh` login.");
}

export interface GithubPrStatusSweepDto {
  checked: { conflict_id: string; outcome: string }[];
  errors: { conflict_id: string; message: string }[];
}

// Round 32: the automatic sweep, called right after a pull instead of
// clicking "Check PR status" per conflict. Same mock-mode boundary as
// everything else here.
export async function checkAllConflictGithubPrStatuses(checkingMemberId: string): Promise<GithubPrStatusSweepDto> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<GithubPrStatusSweepDto>("check_all_conflict_github_pr_statuses", { checkingMemberId });
  }
  throw new Error("Checking GitHub PR statuses isn't available in mock mode, it needs the real engine and a real `gh` login.");
}

// ---- git sync (round 22, Phase 8) ------------------------------------
//
// No SOLEXA server, no SOLEXA accounts, `repoPath` is a directory the
// user already `git clone`d themselves. See solexa_domain::sync for the
// full rationale.

export async function getGitSyncStatus(): Promise<GitSyncStatusDto | null> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("get_git_sync_status");
  }
  return mock.getGitSyncStatus();
}

export async function linkGitRepo(repoPath: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("link_git_repo", { repoPath });
    return;
  }
  mock.linkGitRepo(repoPath);
}

export async function unlinkGitRepo(): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("unlink_git_repo");
    return;
  }
  mock.unlinkGitRepo();
}

export async function pushToGit(): Promise<GitPushResultDto> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("push_to_git");
  }
  return mock.pushToGit();
}

export async function pullFromGit(): Promise<GitPullResultDto> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("pull_from_git");
  }
  return mock.pullFromGit();
}

// ---- pending sync changes and roles (round 46) ------------------------
//
// The review gate on pull_from_git, see solexa-domain::role's doc
// comment for the full rationale. Roles are pure Member-side data with
// a full mock implementation; PendingSyncChange follows the exact same
// mock-mode boundary as resolveSyncConflict just above (a real one only
// ever arises from a real git pull between two independent devices).

export async function setRequireReviewForIncomingChanges(enabled: boolean): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_require_review_for_incoming_changes", { enabled });
    return;
  }
  mock.setRequireReviewForIncomingChanges(enabled);
}

export async function listPendingSyncChanges(): Promise<PendingSyncChangeDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_pending_sync_changes");
  }
  return mock.listPendingSyncChanges();
}

export async function applyPendingSyncChange(pendingChangeId: string, decidingMemberId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("apply_pending_sync_change", { pendingChangeId, decidingMemberId });
    return;
  }
  throw new Error("Applying a pending sync change isn't available in mock mode, it needs a real git pull between two real devices.");
}

export async function denyPendingSyncChange(pendingChangeId: string, decidingMemberId: string, reason?: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("deny_pending_sync_change", { pendingChangeId, decidingMemberId, reason: reason ?? null });
    return;
  }
  throw new Error("Denying a pending sync change isn't available in mock mode, it needs a real git pull between two real devices.");
}

export async function createRole(name: string, permissions: PermissionName[]): Promise<RoleDto> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("create_role", { name, permissions });
  }
  return mock.createRole(name, permissions);
}

export async function updateRole(roleId: string, name: string, permissions: PermissionName[]): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("update_role", { roleId, name, permissions });
    return;
  }
  mock.updateRole(roleId, name, permissions);
}

/** Round 55: whether this project makes deciding a Decision / resolving a
 * Conflict require a role granting Approve. Off by default. */
export async function getEnforceApproverRoles(): Promise<boolean> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("get_enforce_approver_roles");
  }
  return mock.getEnforceApproverRoles();
}

/** Turning it ON rejects if no member holds Approve (it would leave
 * nothing resolvable); turning it OFF always works. */
export async function setEnforceApproverRoles(enabled: boolean): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_enforce_approver_roles", { enabled });
    return;
  }
  mock.setEnforceApproverRoles(enabled);
}

export async function listRoles(): Promise<RoleDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_roles");
  }
  return mock.listRoles();
}

export async function assignRole(memberId: string, roleId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("assign_role", { memberId, roleId });
    return;
  }
  mock.assignRole(memberId, roleId);
}

export async function unassignRole(memberId: string, roleId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("unassign_role", { memberId, roleId });
    return;
  }
  mock.unassignRole(memberId, roleId);
}

// Round 47: extends the role system to Specialist.
export async function assignRoleToSpecialist(specialistId: string, roleId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("assign_role_to_specialist", { specialistId, roleId });
    return;
  }
  mock.assignRoleToSpecialist(specialistId, roleId);
}

export async function unassignRoleFromSpecialist(specialistId: string, roleId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("unassign_role_from_specialist", { specialistId, roleId });
    return;
  }
  mock.unassignRoleFromSpecialist(specialistId, roleId);
}

// Round 49: cancels a task. Real engine: interrupts it while waiting
// on hardware capacity or before it reaches that wait; refuses with a
// clear error once it's generating. Mock mode mirrors the same boundary
// (its "running" state is the analogue of "generating").
export async function cancelTask(taskId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("cancel_task", { taskId });
    return;
  }
  mock.cancelTask(taskId);
}

export async function setTaskPriority(taskId: string, priority: number): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_task_priority", { taskId, priority });
    return;
  }
  mock.setTaskPriority(taskId, priority);
}

// Round 44: `deadline` is an RFC 3339 timestamp string, or null to
// clear an existing deadline.
export async function setTaskDeadline(taskId: string, deadline: string | null): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("set_task_deadline", { taskId, deadline });
    return;
  }
  mock.setTaskDeadline(taskId, deadline);
}

export async function listKnowledgeItems(): Promise<KnowledgeItemDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_knowledge_items");
  }
  return mock.listKnowledgeItems();
}

export async function addKnowledgeItem(
  kind: string,
  title: string,
  content: string,
  tags: string[],
): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("add_knowledge_item", { kind, title, content, tags });
    return;
  }
  mock.addKnowledgeItem(kind, title, content, tags);
}

export async function archiveKnowledgeItem(itemId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("archive_knowledge_item", { itemId });
    return;
  }
  mock.archiveKnowledgeItem(itemId);
}

export async function listDecisions(): Promise<DecisionDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_decisions");
  }
  return mock.listDecisions();
}

export async function addDecision(question: string, options: string[], tags: string[]): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("add_decision", { question, options, tags });
    return;
  }
  mock.addDecision(question, options, tags);
}

export async function resolveDecision(
  decisionId: string,
  resolution: string,
  approvingMemberId: string,
): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("resolve_decision", { decisionId, resolution, approvingMemberId });
    return;
  }
  mock.resolveDecision(decisionId, resolution, approvingMemberId);
}

// Round 33: mirrors a Decision as a GitHub Issue, visibility only,
// same mock-mode boundary as every other GitHub-touching function
// (needs a real linked repo and a real `gh` login).
export async function sendDecisionToGithub(decisionId: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<string>("send_decision_to_github", { decisionId });
  }
  throw new Error("Sending a decision to GitHub isn't available in mock mode, it needs the real engine and a real `gh` login.");
}

// ---- projects (round 35) ---------------------------------------------
//
// Mock mode stays deliberately single-project: the mock engine is one
// in-memory object with one team and one roster, so pretending it could
// switch between several would be exactly the fake scaffolding this
// codebase has avoided everywhere else. listProjects returns the one
// simulated project so the switcher renders honestly; the two mutating
// calls say plainly that they need the real engine.

export async function listProjects(): Promise<ProjectDto[]> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke("list_projects");
  }
  return [{ id: "mock-project", name: "SOLEXA Sandbox", description: "the mock-mode demo project", is_active: true }];
}

export async function createProject(name: string, description: string): Promise<string> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    return await invoke<string>("create_project", { name, description });
  }
  throw new Error("Creating a project isn't available in mock mode, it needs the real engine.");
}

export async function switchProject(projectId: string): Promise<void> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("switch_project", { projectId });
    return;
  }
  throw new Error("Switching projects isn't available in mock mode, it needs the real engine.");
}

/** Live event stream. Returns an unsubscribe function. */
export async function subscribeToEvents(
  onEvent: (event: EventDto) => void,
): Promise<() => void> {
  if (isTauri) {
    const { listen } = await import("@tauri-apps/api/event");
    const unlisten = await listen<EventDto>("solexa://event", (e) => onEvent(e.payload));
    return unlisten;
  }
  return mock.subscribe(onEvent);
}

// ---------------------------------------------------------------------
// Mock mode, see file header. Mirrors solexa-coordinator's event
// sequence closely enough to be a faithful preview, not a fiction.
// ---------------------------------------------------------------------

const MOCK_MODELS: ModelWithStatusDto[] = [
  {
    id: "mock-general",
    name: "mock-general",
    provider: "mock",
    parameter_count_billions: null,
    context_capacity_tokens: 8192,
    tool_support: false,
    local: true,
    enabled: true,
  },
];

const MOCK_SPECIALISTS: SpecialistDto[] = [
  {
    id: "mock-general-specialist",
    name: "General Specialist",
    role: "generalist",
    domain: "general project work",
    capabilities: ["general", "backend", "design", "security", "audio", "qa"],
    benchmark: { verification_passed_count: 0, verification_failed_count: 0, success_rate: 0.5 },
  },
  {
    id: "mock-sql-specialist",
    name: "SQL Specialist",
    role: "database engineer",
    domain: "sql and schema design",
    capabilities: ["sql"],
    benchmark: { verification_passed_count: 0, verification_failed_count: 0, success_rate: 0.5 },
  },
];

const MOCK_MEMBERS: MemberDto[] = [
  {
    id: "mock-human",
    display_name: "You",
    description: "",
    member_type: "human",
    specialist_ids: [],
    deleted_at: null,
    role_ids: [],
  },
  {
    id: "mock-backend-bot",
    display_name: "Backend Bot",
    description: "runs the General Specialist",
    member_type: "ai",
    specialist_ids: [MOCK_SPECIALISTS[0].id],
    deleted_at: null,
    role_ids: [],
  },
  {
    id: "mock-sql-bot",
    display_name: "SQL Bot",
    description: "runs the SQL Specialist",
    member_type: "ai",
    specialist_ids: [MOCK_SPECIALISTS[1].id],
    deleted_at: null,
    role_ids: [],
  },
];

const MOCK_TEAM: TeamDto = {
  id: "mock-team",
  name: "SOLEXA Sandbox Team",
  mission: "handle whatever comes in",
  member_ids: MOCK_MEMBERS.map((m) => m.id),
  parent_team_id: null,
  deleted_at: null,
};

const MOCK_SEQUENCE: Array<{ type: EventDto["event_type"]; delayMs: number; payload?: Record<string, unknown> }> = [
  { type: "request_received", delayMs: 0 },
  { type: "planner_started", delayMs: 120 },
  { type: "task_created", delayMs: 220 },
  { type: "planner_completed", delayMs: 120 },
  { type: "task_assigned", delayMs: 200 },
  { type: "context_packaged", delayMs: 160 },
  // Adopted from a parallel build (its Resource Center): gives the
  // Resource Center panel something genuine to react to in mock mode.
  // `resource_acquired`/`resource_released` are placed right AFTER the
  // event that actually flips `TaskState` in `applyStateTransition`
  // (`model_invoked` -> running, `result_submitted` -> review) rather
  // than mirroring the real Coordinator's literal emission order,
  // this mock's `getResourceSnapshot` derives running/queued from
  // `TaskState`, so firing the refresh-triggering event before the
  // state it reads has moved would show a stale reading for one tick.
  // That ordering bug was found by the parallel build's own Playwright
  // run against the live mock engine, not assumed.
  { type: "model_invoked", delayMs: 220 },
  { type: "resource_acquired", delayMs: 0 },
  // Round 39: a few sample chunks standing in for what the real engine
  // sends live via ModelTokenGenerated (see EventBus::publish_ephemeral)
  // as Ollama actually generates, enough for ExecutionTrace's
  // coalescing-into-one-growing-node behavior to be genuinely exercised
  // in mock mode, not just reasoned about.
  { type: "model_token_generated", delayMs: 160, payload: { chunk: "Reviewing", chunk_index: 1 } },
  { type: "model_token_generated", delayMs: 70, payload: { chunk: " the auth module", chunk_index: 2 } },
  { type: "model_token_generated", delayMs: 70, payload: { chunk: " and drafting", chunk_index: 3 } },
  { type: "model_token_generated", delayMs: 70, payload: { chunk: " a fix.", chunk_index: 4 } },
  { type: "model_completed", delayMs: 140 },
  { type: "result_submitted", delayMs: 120 },
  { type: "resource_released", delayMs: 0 },
  { type: "verification_started", delayMs: 160 },
  { type: "verified", delayMs: 220 },
  { type: "task_completed", delayMs: 120 },
];

function uuid(): string {
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    const v = c === "x" ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}

/**
 * Note: this mock only simulates the single-task pipeline. The real
 * engine's Phase 2 decomposition (multi-capability requests splitting
 * into a parent + per-capability subtasks, see solexa-coordinator) is
 * not reproduced here; every mock request always resolves to one task
 * assigned to the General Specialist. Good enough for previewing the
 * UI's shape; not a claim that this mirrors decomposition behavior.
 */
class MockEngine {
  private tasks = new Map<string, TaskDto>();
  private events = new Map<string, EventDto[]>();
  private listeners = new Set<(e: EventDto) => void>();
  private knowledgeItems: KnowledgeItemDto[] = [];
  private decisions: DecisionDto[] = [];
  // Unified, mutable rosters, seeded from the MOCK_* constants above
  // but never mutating those constants directly, so create/edit/archive
  // all operate on the same objects `listSpecialists`/`listMembers`/
  // `listTeams` return, whether a specialist came from the seed data or
  // was created this session.
  private specialists: SpecialistDto[] = MOCK_SPECIALISTS.map((s) => ({ ...s, active: true }));
  private members: MemberDto[] = [...MOCK_MEMBERS];
  private teams: TeamDto[] = [{ ...MOCK_TEAM }];
  // Round 46: unlike PendingSyncChange (see listPendingSyncChanges),
  // roles need no real git pull to demonstrate, pure Member-side data,
  // so this gets a full, real mock implementation rather than an empty
  // stub or a throw.
  private roles: RoleDto[] = [];
  // Round 49: tasks whose scripted sequence must stop (see cancelTask).
  private cancelledTasks = new Set<string>();
  private roleCounter = 0;
  private registryEntries: Array<RegistryEntryDto & { packageJson: string }> = [];
  private gitSync: GitSyncStatusDto | null = null;
  // Round 41: per-project model configuration, mock-mode counterpart to
  // the `project_disabled_models` table, a model id in here is
  // disabled for the (one, implicit) project mock mode simulates.
  private disabledModelIds = new Set<string>();
  private mockCommitCounter = 0;

  async submitRequest(text: string, priority?: number): Promise<SubmissionDto> {
    const taskId = uuid();
    const correlationId = uuid();
    const now = new Date().toISOString();
    const title = text.split("\n")[0].slice(0, 80) || "Untitled request";

    const task: TaskDto = {
      id: taskId,
      title,
      description: text,
      state: "created",
      assigned_specialist_id: null,
      required_capabilities: ["general"],
      priority: priority ?? 5,
      deadline: null,
      result: null,
      confidence: null,
      verification: null,
      created_at: now,
      updated_at: now,
    };
    this.tasks.set(taskId, task);
    this.events.set(correlationId, []);

    void this.runSequence(taskId, correlationId, text);

    return { task_id: taskId, correlation_id: correlationId };
  }

  /** Mirrors `Coordinator::cancel_task`. Since round 53 the real engine
   * interrupts a task that is already generating, so this mock no longer
   * refuses the "running" state (model_invoked → result_submitted, its
   * analogue of generating): `runSequence` checks the cancelled set before
   * every scripted step, so the sequence stops at its next step and the
   * result never lands. Only finished tasks are refused. */
  cancelTask(taskId: string): void {
    const task = this.tasks.get(taskId);
    if (!task) throw new Error(`task ${taskId} not found`);
    if (["completed", "failed", "cancelled", "verified"].includes(task.state)) {
      throw new Error(`task ${taskId} can't be cancelled, it's already ${task.state}`);
    }
    this.cancelledTasks.add(taskId);
    task.state = "cancelled";
  }

  setTaskPriority(taskId: string, priority: number): void {
    const task = this.tasks.get(taskId);
    if (task) task.priority = priority;
  }

  setTaskDeadline(taskId: string, deadline: string | null): void {
    const task = this.tasks.get(taskId);
    if (task) task.deadline = deadline;
  }

  async listKnowledgeItems(): Promise<KnowledgeItemDto[]> {
    // A fresh array each call, not the live internal one, returning
    // the same reference after mutating it in place would make React's
    // setState bail out (Object.is sees "no change") even though the
    // content genuinely changed. Exactly this bug is why the Project
    // Brain panel's count wouldn't update after adding an item until
    // this was fixed.
    return [...this.knowledgeItems];
  }

  addKnowledgeItem(kind: string, title: string, content: string, tags: string[]): void {
    this.knowledgeItems.push({
      id: uuid(),
      kind: kind as KnowledgeItemDto["kind"],
      title,
      content,
      status: "accepted",
      tags,
      created_at: new Date().toISOString(),
    });
  }

  archiveKnowledgeItem(itemId: string): void {
    const item = this.knowledgeItems.find((i) => i.id === itemId);
    if (item) item.status = "archived";
  }

  async listDecisions(): Promise<DecisionDto[]> {
    return [...this.decisions];
  }

  addDecision(question: string, options: string[], tags: string[]): void {
    this.decisions.push({
      id: uuid(),
      question,
      options,
      resolution: null,
      approved_by: null,
      status: "open",
      tags,
      created_at: new Date().toISOString(),
      github_issue_url: null,
    });
  }

  resolveDecision(decisionId: string, resolution: string, approvingMemberId: string): void {
    const decision = this.decisions.find((d) => d.id === decisionId);
    if (!decision) return;
    if (this.enforceApproverRoles && !this.memberHasPermission(approvingMemberId, "approve")) {
      throw new Error(
        `member ${approvingMemberId} isn't authorized to resolve a decision, this project enforces roles, so it needs a role with Permission::Approve`,
      );
    }
    if (!decision.options.includes(resolution)) {
      throw new Error(`"${resolution}" is not one of this decision's declared options.`);
    }
    const member = MOCK_MEMBERS.find((m) => m.id === approvingMemberId);
    decision.resolution = resolution;
    decision.approved_by = member?.display_name ?? approvingMemberId;
    decision.status = "resolved";
  }

  private async runSequence(taskId: string, correlationId: string, text: string) {
    let previousId: string | null = null;
    for (const step of MOCK_SEQUENCE) {
      await delay(step.delayMs);
      if (this.cancelledTasks.has(taskId)) return;
      const event: EventDto = {
        event_id: uuid(),
        timestamp: new Date().toISOString(),
        actor_id: step.type === "request_received" ? "human" : "coordinator",
        actor_type: step.type === "request_received" ? "human" : "system",
        event_type: step.type,
        payload: step.payload ? { task_id: taskId, ...step.payload } : mockPayload(step.type, taskId, text),
        correlation_id: correlationId,
        causation_id: previousId,
      };
      previousId = event.event_id;
      this.events.get(correlationId)?.push(event);
      this.listeners.forEach((fn) => fn(event));
      this.applyStateTransition(taskId, step.type);
    }
  }

  private applyStateTransition(taskId: string, type: EventDto["event_type"]) {
    const task = this.tasks.get(taskId);
    if (!task) return;
    const next: Partial<Record<EventDto["event_type"], TaskState>> = {
      task_created: "planned",
      task_assigned: "assigned",
      context_packaged: "assigned",
      model_invoked: "running",
      result_submitted: "review",
      verified: "review",
      task_completed: "completed",
    };
    const nextState = next[type];
    if (nextState) task.state = nextState;
    if (type === "task_assigned") task.assigned_specialist_id = MOCK_SPECIALISTS[0].id;
    if (type === "verified") {
      task.verification = {
        method: "deterministic_rule",
        passed: true,
        details: "result content is non-empty",
        verified_at: new Date().toISOString(),
      };
      task.confidence = {
        knowledge: "medium",
        reasoning: "medium",
        evidence: "medium",
        requirement_coverage: "medium",
        tool_reliability: "high",
        completeness: "high",
      };
      task.result = {
        content: `[General Specialist] Received task:\n"${task.description}"\n\n(mock preview, no engine attached)`,
        produced_by_specialist_id: MOCK_SPECIALISTS[0].id,
        produced_by_model_id: "mock-general",
        tool_calls: [],
      };
    }
    task.updated_at = new Date().toISOString();
  }

  async listTasks(): Promise<TaskDto[]> {
    return [...this.tasks.values()].sort((a, b) => a.created_at.localeCompare(b.created_at));
  }

  async getTrace(correlationId: string): Promise<EventDto[]> {
    return this.events.get(correlationId) ?? [];
  }

  async listSpecialists(): Promise<SpecialistDto[]> {
    return [...this.specialists];
  }

  async listModels(): Promise<ModelDto[]> {
    // Round 41: mirrors the real list_models command, only what this
    // project has available, not the full registry.
    return MOCK_MODELS.filter((m) => !this.disabledModelIds.has(m.id));
  }

  async listModelsWithStatus(): Promise<ModelWithStatusDto[]> {
    return MOCK_MODELS.map((m) => ({ ...m, enabled: !this.disabledModelIds.has(m.id) }));
  }

  setModelEnabled(modelId: string, enabled: boolean): void {
    if (enabled) {
      this.disabledModelIds.delete(modelId);
    } else {
      this.disabledModelIds.add(modelId);
    }
  }

  // Round 60: the mock's live budget (starts at `ResourceBudget::default()`).
  private resourceBudget = { vram_mb: 24_000, ram_mb: 64_000, max_concurrent: 4 };
  // The mock's per-task footprint, see `getResourceSnapshot`.
  private static readonly PER_TASK_VRAM_MB = 6_000;
  private static readonly PER_TASK_RAM_MB = 8_000;

  /** Mirrors `Coordinator::reset_resource_budget`: the default goes through the
   * same checks as any resize, so it can be refused the same way. */
  resetResourceBudget(): void {
    this.setResourceBudget(24_000, 64_000, 4); // `ResourceBudget::default()`
  }

  /** Mirrors `ResourceManager::set_budget`: same rules, same messages. */
  setResourceBudget(vramMb: number, ramMb: number, maxConcurrent: number): void {
    const whole = (n: number) => Number.isInteger(n) && n >= 0;
    if (!whole(vramMb) || !whole(ramMb)) {
      throw new Error("invalid resource budget: memory must be a whole number of MB, 0 or more");
    }
    if (!Number.isInteger(maxConcurrent) || maxConcurrent < 1) {
      throw new Error("invalid resource budget: max_concurrent must be at least 1");
    }
    if (maxConcurrent > 256) {
      throw new Error("invalid resource budget: max_concurrent is above the supported limit");
    }
    const queued = [...this.tasks.values()].filter((t) => t.state === "planned" || t.state === "assigned").length;
    if (queued > 0 && (MockEngine.PER_TASK_VRAM_MB > vramMb || MockEngine.PER_TASK_RAM_MB > ramMb)) {
      throw new Error(
        `${queued} queued task(s) need more memory than the proposed budget, cancel them or choose a larger budget`,
      );
    }
    this.resourceBudget = { vram_mb: vramMb, ram_mb: ramMb, max_concurrent: maxConcurrent };
  }

  // Adopted from a parallel build. This mock has no independent
  // resource bookkeeping the way the real `ResourceManager` does, so
  // `running`/`queued` are derived from real mock task state: "running"
  // once a task's own `model_invoked` step has fired, "queued" while
  // it's been submitted (planned/assigned) but hasn't gotten there yet.
  // A looser analogy to real VRAM contention than the Rust side, but a
  // live, genuine reading of this engine's own state rather than
  // fabricated numbers. `budget_*` mirrors `ResourceBudget::default()`.
  async getResourceSnapshot(): Promise<ResourceSnapshotDto> {
    const states = [...this.tasks.values()].map((t) => t.state);
    const running = states.filter((s) => s === "running").length;
    const queued = states.filter((s) => s === "planned" || s === "assigned").length;
    const perTaskVramMb = MockEngine.PER_TASK_VRAM_MB;
    const perTaskRamMb = MockEngine.PER_TASK_RAM_MB;
    return {
      used_vram_mb: running * perTaskVramMb,
      used_ram_mb: running * perTaskRamMb,
      running,
      queued,
      budget_vram_mb: this.resourceBudget.vram_mb,
      budget_ram_mb: this.resourceBudget.ram_mb,
      max_concurrent: this.resourceBudget.max_concurrent,
    };
  }

  createSpecialist(input: CreateSpecialistInput): CreateSpecialistResult {
    assertValidGenerationLimits(input.maxTokens, input.temperature);
    const specialistId = uuid();
    const memberId = uuid();
    this.specialists.push({
      id: specialistId,
      name: input.name,
      role: input.role,
      domain: input.domain,
      capabilities: input.capabilities,
      preferred_model_ids: input.preferredModelIds,
      tools: input.tools,
      knowledge_scope: input.knowledgeScope,
      permissions: input.permissions,
      escalate_on_weak_confidence: input.escalateOnWeakConfidence,
      escalate_on_verification_failure: input.escalateOnVerificationFailure,
      requires_model_review: input.requiresModelReview,
      requires_specialist_review: input.requiresSpecialistReview,
      max_tokens: input.maxTokens ?? null,
      temperature: input.temperature ?? null,
      active: true,
      benchmark: { verification_passed_count: 0, verification_failed_count: 0, success_rate: 0.5 },
    });
    this.members.push({
      id: memberId,
      display_name: input.memberDisplayName.trim() || `${input.name} Bot`,
      description: `runs the ${input.name}`,
      member_type: "ai",
      specialist_ids: [specialistId],
      deleted_at: null,
      role_ids: [],
    });
    if (input.addToTeamId) {
      const team = this.teams.find((t) => t.id === input.addToTeamId);
      if (team && !team.member_ids.includes(memberId)) {
        team.member_ids.push(memberId);
      }
    }
    return { specialist_id: specialistId, member_id: memberId };
  }

  /** Mirrors `Coordinator::edit_specialist`'s validation and its
   * `active`-preservation rule (see the comment on that function) so
   * the mock can't drift into a false preview of what the real engine
   * would do. */
  updateSpecialist(input: UpdateSpecialistInput): void {
    assertValidGenerationLimits(input.maxTokens, input.temperature);
    const specialist = this.specialists.find((s) => s.id === input.specialistId);
    if (!specialist) {
      throw new Error(`specialist ${input.specialistId} not found`);
    }
    if (!input.name.trim()) {
      throw new Error("specialist name must not be empty");
    }
    if (input.capabilities.length === 0) {
      throw new Error(
        "specialist must declare at least one capability, or it can never be matched to a task",
      );
    }
    specialist.name = input.name;
    specialist.role = input.role;
    specialist.domain = input.domain;
    specialist.capabilities = input.capabilities;
    specialist.preferred_model_ids = input.preferredModelIds;
    specialist.tools = input.tools;
    specialist.knowledge_scope = input.knowledgeScope;
    specialist.permissions = input.permissions;
    specialist.escalate_on_weak_confidence = input.escalateOnWeakConfidence;
    specialist.escalate_on_verification_failure = input.escalateOnVerificationFailure;
    specialist.requires_model_review = input.requiresModelReview;
    specialist.requires_specialist_review = input.requiresSpecialistReview;
    // Round 52: an edit replaces the limits, so an omitted one is cleared.
    specialist.max_tokens = input.maxTokens ?? null;
    specialist.temperature = input.temperature ?? null;
    // `active` is deliberately untouched here, see the function comment.
  }

  setSpecialistActive(specialistId: string, active: boolean): void {
    const specialist = this.specialists.find((s) => s.id === specialistId);
    if (!specialist) {
      throw new Error(`specialist ${specialistId} not found`);
    }
    specialist.active = active;
  }

  /** Mirrors the real Tauri command's JSON shape (nested
   * `escalation_policy`, not the flatter fields `SpecialistDto` uses
   * elsewhere in this mock) so a package exported from mock mode looks
   * like one a real build would actually produce. */
  exportSpecialistPackage(specialistId: string): string {
    const specialist = this.specialists.find((s) => s.id === specialistId);
    if (!specialist) {
      throw new Error(`specialist ${specialistId} not found`);
    }
    const benchmark = specialist.benchmark ?? { verification_passed_count: 0, verification_failed_count: 0 };
    const pkg = {
      package_format_version: 1,
      specialist: {
        id: specialist.id,
        name: specialist.name,
        role: specialist.role,
        domain: specialist.domain,
        capabilities: specialist.capabilities,
        preferred_model_ids: specialist.preferred_model_ids ?? [],
        tools: specialist.tools ?? [],
        knowledge_scope: specialist.knowledge_scope ?? [],
        permissions: specialist.permissions ?? [],
        output_schema: specialist.output_schema ?? null,
        escalation_policy: {
          escalate_on_weak_confidence: specialist.escalate_on_weak_confidence ?? true,
          escalate_on_verification_failure: specialist.escalate_on_verification_failure ?? true,
          requires_model_review: specialist.requires_model_review ?? false,
          requires_specialist_review: specialist.requires_specialist_review ?? false,
        },
        active: specialist.active !== false,
        max_tokens: specialist.max_tokens ?? null,
        temperature: specialist.temperature ?? null,
      },
      benchmark: {
        specialist_id: specialist.id,
        verification_passed_count: benchmark.verification_passed_count,
        verification_failed_count: benchmark.verification_failed_count,
      },
    };
    return JSON.stringify(pkg, null, 2);
  }

  /** Mirrors `Coordinator::import_specialist_package`'s validation and
   * its always-fresh-id, always-active rule. */
  importSpecialistPackage(packageJson: string): ImportSpecialistPackageResult {
    let parsed: {
      package_format_version?: number;
      specialist?: SpecialistPackageSpecialistLike;
      benchmark?: { verification_passed_count?: number; verification_failed_count?: number };
    };
    try {
      parsed = JSON.parse(packageJson);
    } catch (err) {
      throw new Error(`not a valid specialist package: ${err instanceof Error ? err.message : String(err)}`);
    }
    if (parsed.package_format_version !== 1) {
      throw new Error(
        `unrecognized specialist package format version ${parsed.package_format_version} (this build understands version 1)`,
      );
    }
    const specialistId = this.pushSpecialistFromPackage(parsed.specialist, parsed.benchmark);
    return { specialist_id: specialistId };
  }

  /** Shared by `importSpecialistPackage` and `importTeamPackage`, the
   * mock's equivalent of the real engine reusing
   * `import_specialist_package` inside `import_team_package` rather
   * than duplicating the validation and construction logic. */
  private pushSpecialistFromPackage(
    src: SpecialistPackageSpecialistLike | undefined,
    benchmarkSrc: { verification_passed_count?: number; verification_failed_count?: number } | undefined,
  ): string {
    if (!src || !src.name?.trim()) {
      throw new Error("specialist name must not be empty");
    }
    if (!src.capabilities || src.capabilities.length === 0) {
      throw new Error("specialist must declare at least one capability, or it can never be matched to a task");
    }
    const passed = benchmarkSrc?.verification_passed_count ?? 0;
    const failed = benchmarkSrc?.verification_failed_count ?? 0;
    const attempts = passed + failed;
    const newId = uuid();
    this.specialists.push({
      id: newId,
      name: src.name,
      role: src.role ?? "",
      domain: src.domain ?? "",
      capabilities: src.capabilities,
      preferred_model_ids: src.preferred_model_ids ?? [],
      tools: src.tools ?? [],
      knowledge_scope: src.knowledge_scope ?? [],
      permissions: src.permissions ?? [],
      output_schema: src.output_schema ?? undefined,
      escalate_on_weak_confidence: src.escalation_policy?.escalate_on_weak_confidence ?? true,
      escalate_on_verification_failure: src.escalation_policy?.escalate_on_verification_failure ?? true,
      requires_model_review: src.escalation_policy?.requires_model_review ?? false,
      requires_specialist_review: src.escalation_policy?.requires_specialist_review ?? false,
      max_tokens: src.max_tokens ?? null,
      temperature: src.temperature ?? null,
      active: true,
      benchmark: {
        verification_passed_count: passed,
        verification_failed_count: failed,
        success_rate: attempts === 0 ? 0.5 : passed / attempts,
      },
    });
    return newId;
  }

  /** Mirrors the real Tauri command's JSON shape, excludes human
   * members, and each AI member's specialist uses the same nested
   * `escalation_policy` shape `exportSpecialistPackage` produces. */
  exportTeamPackage(teamId: string): string {
    const team = this.teams.find((t) => t.id === teamId);
    if (!team) {
      throw new Error(`team ${teamId} not found`);
    }
    const members = [];
    for (const memberId of team.member_ids) {
      const member = this.members.find((m) => m.id === memberId);
      if (!member || member.member_type !== "ai") continue;
      for (const specialistId of member.specialist_ids) {
        const specialist = this.specialists.find((s) => s.id === specialistId);
        if (!specialist) continue;
        const benchmark = specialist.benchmark ?? { verification_passed_count: 0, verification_failed_count: 0 };
        members.push({
          display_name: member.display_name,
          description: member.description,
          specialist: {
            package_format_version: 1,
            specialist: {
              id: specialist.id,
              name: specialist.name,
              role: specialist.role,
              domain: specialist.domain,
              capabilities: specialist.capabilities,
              preferred_model_ids: specialist.preferred_model_ids ?? [],
              tools: specialist.tools ?? [],
              knowledge_scope: specialist.knowledge_scope ?? [],
              permissions: specialist.permissions ?? [],
              output_schema: specialist.output_schema ?? null,
              escalation_policy: {
                escalate_on_weak_confidence: specialist.escalate_on_weak_confidence ?? true,
                escalate_on_verification_failure: specialist.escalate_on_verification_failure ?? true,
                requires_model_review: specialist.requires_model_review ?? false,
                requires_specialist_review: specialist.requires_specialist_review ?? false,
              },
              active: specialist.active !== false,
        max_tokens: specialist.max_tokens ?? null,
        temperature: specialist.temperature ?? null,
            },
            benchmark: {
              specialist_id: specialist.id,
              verification_passed_count: benchmark.verification_passed_count,
              verification_failed_count: benchmark.verification_failed_count,
            },
          },
        });
      }
    }
    const pkg = {
      package_format_version: 1,
      name: team.name,
      mission: team.mission,
      members,
    };
    return JSON.stringify(pkg, null, 2);
  }

  /** Mirrors `Coordinator::import_team_package`, registers a new
   * team, then for each packaged AI member reuses
   * `pushSpecialistFromPackage` (the same validation
   * `importSpecialistPackage` uses) and creates a fresh AI member
   * running it. */
  importTeamPackage(packageJson: string): ImportTeamPackageResult {
    let parsed: {
      package_format_version?: number;
      name?: string;
      mission?: string;
      members?: Array<{
        display_name?: string;
        description?: string;
        specialist?: {
          specialist?: SpecialistPackageSpecialistLike;
          benchmark?: { verification_passed_count?: number; verification_failed_count?: number };
        };
      }>;
    };
    try {
      parsed = JSON.parse(packageJson);
    } catch (err) {
      throw new Error(`not a valid team package: ${err instanceof Error ? err.message : String(err)}`);
    }
    if (parsed.package_format_version !== 1) {
      throw new Error(
        `unrecognized team package format version ${parsed.package_format_version} (this build understands version 1)`,
      );
    }
    if (!parsed.name?.trim()) {
      throw new Error("team name must not be empty");
    }
    const memberIds: string[] = [];
    for (const memberPkg of parsed.members ?? []) {
      const specialistId = this.pushSpecialistFromPackage(
        memberPkg.specialist?.specialist,
        memberPkg.specialist?.benchmark,
      );
      const memberId = uuid();
      this.members.push({
        id: memberId,
        display_name: memberPkg.display_name ?? "",
        description: memberPkg.description ?? "",
        member_type: "ai",
        specialist_ids: [specialistId],
        deleted_at: null,
        role_ids: [],
      });
      memberIds.push(memberId);
    }
    const teamId = uuid();
    this.teams.push({
      id: teamId,
      name: parsed.name,
      mission: parsed.mission ?? "",
      member_ids: memberIds,
      parent_team_id: null,
      deleted_at: null,
    });
    return { team_id: teamId };
  }

  /** Reuses `exportSpecialistPackage` directly, saving to the
   * registry is exactly exporting, then keeping the result, the same
   * composition-over-duplication discipline the real engine holds. */
  saveSpecialistToRegistry(specialistId: string): SaveToRegistryResult {
    const specialist = this.specialists.find((s) => s.id === specialistId);
    if (!specialist) {
      throw new Error(`specialist ${specialistId} not found`);
    }
    const packageJson = this.exportSpecialistPackage(specialistId);
    const entryId = uuid();
    this.registryEntries.push({
      id: entryId,
      kind: "specialist",
      name: specialist.name,
      domain: specialist.domain,
      capabilities: [...specialist.capabilities],
      added_at: new Date().toISOString(),
      packageJson,
    });
    return { registry_entry_id: entryId };
  }

  /** Reuses `exportTeamPackage`; capabilities are the union across
   * every packaged AI member's specialist. */
  saveTeamToRegistry(teamId: string): SaveToRegistryResult {
    const team = this.teams.find((t) => t.id === teamId);
    if (!team) {
      throw new Error(`team ${teamId} not found`);
    }
    const packageJson = this.exportTeamPackage(teamId);
    const parsed = JSON.parse(packageJson) as {
      members?: Array<{ specialist?: { specialist?: { capabilities?: string[] } } }>;
    };
    const capabilities: string[] = [];
    for (const member of parsed.members ?? []) {
      for (const cap of member.specialist?.specialist?.capabilities ?? []) {
        if (!capabilities.includes(cap)) capabilities.push(cap);
      }
    }
    const entryId = uuid();
    this.registryEntries.push({
      id: entryId,
      kind: "team",
      name: team.name,
      domain: "",
      capabilities,
      added_at: new Date().toISOString(),
      packageJson,
    });
    return { registry_entry_id: entryId };
  }

  async listRegistryEntries(): Promise<RegistryEntryDto[]> {
    return this.registryEntries
      .slice()
      .sort((a, b) => (a.added_at < b.added_at ? 1 : -1))
      .map(({ packageJson: _packageJson, ...rest }) => rest);
  }

  async getRegistryEntryPackageJson(entryId: string): Promise<string> {
    const entry = this.registryEntries.find((e) => e.id === entryId);
    if (!entry) {
      throw new Error(`registry entry ${entryId} not found`);
    }
    return entry.packageJson;
  }

  /** Reuses `importSpecialistPackage`/`importTeamPackage` directly,
   * installing is exactly deserializing the saved package and handing
   * it to the same import path a pasted or file-loaded package goes
   * through. */
  installFromRegistry(entryId: string): InstallFromRegistryResult {
    const entry = this.registryEntries.find((e) => e.id === entryId);
    if (!entry) {
      throw new Error(`registry entry ${entryId} not found`);
    }
    if (entry.kind === "specialist") {
      const result = this.importSpecialistPackage(entry.packageJson);
      return { kind: "specialist", id: result.specialist_id };
    }
    const result = this.importTeamPackage(entry.packageJson);
    return { kind: "team", id: result.team_id };
  }

  /** A genuine, permanent delete, no archive-style soft-delete for a
   * registry entry, matching the real engine. */
  removeFromRegistry(entryId: string): void {
    const index = this.registryEntries.findIndex((e) => e.id === entryId);
    if (index === -1) {
      throw new Error(`registry entry ${entryId} not found`);
    }
    this.registryEntries.splice(index, 1);
  }

  async listMembers(): Promise<MemberDto[]> {
    return [...this.members];
  }

  /** Mirrors `Coordinator::edit_member`'s validation and its
   * `member_type`/`role_ids`-preservation rules, this mutates the
   * existing object in place rather than replacing it, so role_ids
   * (like member_type) is naturally untouched without needing a
   * separate override the way the real Rust struct-literal call site
   * does. */
  updateMember(input: UpdateMemberInput): void {
    const member = this.members.find((m) => m.id === input.memberId);
    if (!member) {
      throw new Error(`member ${input.memberId} not found`);
    }
    if (!input.displayName.trim()) {
      throw new Error("member display name must not be empty");
    }
    member.display_name = input.displayName;
    member.description = input.description;
    member.specialist_ids = input.specialistIds;
    // `member_type` is deliberately untouched, identity, not editable.
  }

  async listTeams(): Promise<TeamDto[]> {
    return this.teams.map((t) => ({ ...t, member_ids: [...t.member_ids] }));
  }

  /** Mirrors `Coordinator::register_team`'s validation. */
  createTeam(input: CreateTeamInput): CreateTeamResult {
    if (!input.name.trim()) {
      throw new Error("team name must not be empty");
    }
    const teamId = uuid();
    this.teams.push({
      id: teamId,
      name: input.name,
      mission: input.mission,
      member_ids: [],
      parent_team_id: input.parentTeamId ?? null,
      deleted_at: null,
    });
    return { team_id: teamId };
  }

  /** Mirrors `Coordinator::edit_team`'s validation. Membership and
   * hierarchy aren't parameters here, matching the engine. */
  updateTeam(input: UpdateTeamInput): void {
    const team = this.teams.find((t) => t.id === input.teamId);
    if (!team) {
      throw new Error(`team ${input.teamId} not found`);
    }
    if (!input.name.trim()) {
      throw new Error("team name must not be empty");
    }
    team.name = input.name;
    team.mission = input.mission;
  }

  /** Mirrors `Coordinator::set_team_parent`'s cycle check, walks the
   * subtree under `teamId` and refuses to move it under itself or any
   * of its own descendants. */
  setTeamParent(teamId: string, newParentId: string | undefined): void {
    const team = this.teams.find((t) => t.id === teamId);
    if (!team) {
      throw new Error(`team ${teamId} not found`);
    }
    if (newParentId) {
      if (newParentId === teamId) {
        throw new Error("a team cannot be its own parent");
      }
      if (!this.teams.find((t) => t.id === newParentId)) {
        throw new Error(`parent team ${newParentId} not found`);
      }
      const descendants = new Set<string>();
      const collect = (id: string) => {
        for (const t of this.teams) {
          if (t.parent_team_id === id && !descendants.has(t.id)) {
            descendants.add(t.id);
            collect(t.id);
          }
        }
      };
      collect(teamId);
      if (descendants.has(newParentId)) {
        throw new Error(`cannot move team under ${newParentId}, this would create a cycle`);
      }
    }
    team.parent_team_id = newParentId ?? null;
  }

  /** Mirrors `Coordinator::add_member_to_team`, idempotent (adding an
   * already-present member is a no-op). */
  addMemberToTeam(teamId: string, memberId: string): void {
    const team = this.teams.find((t) => t.id === teamId);
    if (team && !team.member_ids.includes(memberId)) {
      team.member_ids.push(memberId);
    }
  }

  /** Mirrors `Coordinator::remove_member_from_team`, idempotent
   * (removing an absent member is a no-op), never deletes the member. */
  removeMemberFromTeam(teamId: string, memberId: string): void {
    const team = this.teams.find((t) => t.id === teamId);
    if (team) {
      team.member_ids = team.member_ids.filter((id) => id !== memberId);
    }
  }

  // ---- soft delete (round 27) ----------------------------------------

  deleteMember(memberId: string): void {
    const member = this.members.find((m) => m.id === memberId);
    if (member) member.deleted_at = new Date().toISOString();
  }

  restoreMember(memberId: string): void {
    const member = this.members.find((m) => m.id === memberId);
    if (member) member.deleted_at = null;
  }

  deleteTeam(teamId: string): void {
    const team = this.teams.find((t) => t.id === teamId);
    if (team) team.deleted_at = new Date().toISOString();
  }

  restoreTeam(teamId: string): void {
    const team = this.teams.find((t) => t.id === teamId);
    if (team) team.deleted_at = null;
  }

  // ---- roles (round 46) ----------------------------------------------
  //
  // "Discord roles" for SOLEXA, full real mock support (unlike
  // PendingSyncChange above), since none of this needs a real git pull
  // to demonstrate.

  createRole(name: string, permissions: PermissionName[]): RoleDto {
    if (!name.trim()) {
      throw new Error("role name must not be empty");
    }
    this.roleCounter += 1;
    const role: RoleDto = { id: `mock-role-${this.roleCounter}`, name, permissions, created_at: new Date().toISOString() };
    this.roles.push(role);
    return role;
  }

  updateRole(roleId: string, name: string, permissions: PermissionName[]): void {
    const role = this.roles.find((r) => r.id === roleId);
    if (!role) {
      throw new Error(`role ${roleId} not found`);
    }
    role.name = name;
    role.permissions = permissions;
  }

  listRoles(): RoleDto[] {
    return [...this.roles];
  }

  // Round 55: mirrors `Coordinator::{enforce_approver_roles,
  // set_enforce_approver_roles, require_approver_permission}`. Only
  // `resolveDecision` is reachable in mock mode (task/sync conflicts need
  // the real engine), so it is the one action gated here.
  private enforceApproverRoles = false;

  private memberHasPermission(memberId: string, permission: PermissionName): boolean {
    const member = this.members.find((m) => m.id === memberId);
    if (!member) return false;
    return member.role_ids.some((rid) => this.roles.find((r) => r.id === rid)?.permissions.includes(permission));
  }

  getEnforceApproverRoles(): boolean {
    return this.enforceApproverRoles;
  }

  setEnforceApproverRoles(enabled: boolean): void {
    if (enabled && !this.members.some((m) => this.memberHasPermission(m.id, "approve"))) {
      throw new Error(
        "no member of this project holds Permission::Approve, so turning on role enforcement would leave every decision and conflict unresolvable, create a role that grants Approve and assign it to a member first",
      );
    }
    this.enforceApproverRoles = enabled;
  }

  /** Idempotent, mirrors `Coordinator::assign_role`. */
  assignRole(memberId: string, roleId: string): void {
    const member = this.members.find((m) => m.id === memberId);
    if (!member) throw new Error(`member ${memberId} not found`);
    if (!this.roles.some((r) => r.id === roleId)) throw new Error(`role ${roleId} not found`);
    if (!member.role_ids.includes(roleId)) member.role_ids = [...member.role_ids, roleId];
  }

  unassignRole(memberId: string, roleId: string): void {
    const member = this.members.find((m) => m.id === memberId);
    if (!member) throw new Error(`member ${memberId} not found`);
    member.role_ids = member.role_ids.filter((id) => id !== roleId);
  }

  // Round 47: extends the role system to Specialist.
  assignRoleToSpecialist(specialistId: string, roleId: string): void {
    const specialist = this.specialists.find((s) => s.id === specialistId);
    if (!specialist) throw new Error(`specialist ${specialistId} not found`);
    if (!this.roles.some((r) => r.id === roleId)) throw new Error(`role ${roleId} not found`);
    const current = specialist.role_ids ?? [];
    if (!current.includes(roleId)) specialist.role_ids = [...current, roleId];
  }

  unassignRoleFromSpecialist(specialistId: string, roleId: string): void {
    const specialist = this.specialists.find((s) => s.id === specialistId);
    if (!specialist) throw new Error(`specialist ${specialistId} not found`);
    specialist.role_ids = (specialist.role_ids ?? []).filter((id) => id !== roleId);
  }

  // ---- git sync (round 22) ------------------------------------------
  //
  // Genuinely honest happy-path plumbing (link/push/pull all really
  // update `gitSync`'s state, exactly what a Playwright test can
  // observe) but not a real git simulation, there's only one browser
  // tab's worth of state here, so there's no independent "remote" to
  // ever diverge from it. Fast-forwards and conflicts are exercised for
  // real in solexa-coordinator's own test suite (with an actual git
  // binary and two independent clones), see the round's README.

  getGitSyncStatus(): GitSyncStatusDto | null {
    return this.gitSync ? { ...this.gitSync } : null;
  }

  linkGitRepo(repoPath: string): void {
    this.gitSync = {
      repo_path: repoPath,
      linked_at: new Date().toISOString(),
      last_synced_commit: null,
      last_synced_at: null,
      local_dirty: true,
      require_review_for_incoming_changes: false,
    };
  }

  unlinkGitRepo(): void {
    if (!this.gitSync) {
      throw new Error("this project has no linked git repo");
    }
    this.gitSync = null;
  }

  /** Round 46: honest happy-path plumbing, same boundary as the rest
   * of this class's git-sync methods, genuinely toggles the stored
   * flag (observable by a Playwright test), but there's no independent
   * "remote" here for it to actually gate anything against. The real
   * gate (pull_from_git queuing a PendingSyncChange instead of
   * applying) is proven in solexa-coordinator's own test suite with a
   * real git binary and two independent clones. */
  setRequireReviewForIncomingChanges(enabled: boolean): void {
    if (!this.gitSync) {
      throw new Error("project has no linked git repo, link one with link_git_repo first");
    }
    this.gitSync.require_review_for_incoming_changes = enabled;
  }

  /** Always empty in mock mode, a PendingSyncChange, like a
   * SyncConflict, only ever arises from a real git pull between two
   * independent devices, which this single-tab mock has never
   * simulated (see the class-level note on the git-sync methods
   * above). */
  listPendingSyncChanges(): PendingSyncChangeDto[] {
    return [];
  }

  pushToGit(): GitPushResultDto {
    if (!this.gitSync) {
      throw new Error("this project has no linked git repo, link one first");
    }
    this.mockCommitCounter += 1;
    const commit = this.mockCommitCounter.toString(16).padStart(7, "0");
    this.gitSync.last_synced_commit = commit;
    this.gitSync.last_synced_at = new Date().toISOString();
    this.gitSync.local_dirty = false;
    return { pushed: true, commit };
  }

  pullFromGit(): GitPullResultDto {
    if (!this.gitSync) {
      throw new Error("this project has no linked git repo, link one first");
    }
    // Nothing to reconcile, see the class-level note above.
    const commit = this.gitSync.last_synced_commit ?? "0000000";
    this.gitSync.last_synced_at = new Date().toISOString();
    return {
      commit,
      had_remote_changes: false,
      members_applied: 0,
      teams_applied: 0,
      specialists_applied: 0,
      knowledge_items_applied: 0,
      decisions_applied: 0,
      conflicts_created: [],
      pending_changes_created: [],
      github_prs_created: [],
      github_pr_errors: [],
    };
  }

  subscribe(onEvent: (e: EventDto) => void): () => void {
    this.listeners.add(onEvent);
    return () => this.listeners.delete(onEvent);
  }
}

function mockPayload(type: EventDto["event_type"], taskId: string, text: string): unknown {
  switch (type) {
    case "request_received":
      return { text };
    case "task_created":
      return { task_id: taskId, title: text.split("\n")[0].slice(0, 80), required_capabilities: ["general"] };
    default:
      return { task_id: taskId };
  }
}

function delay(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

const mock = new MockEngine();
