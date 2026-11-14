// Mirrors the JSON shapes the Rust side sends (see apps/desktop/src-tauri/src/main.rs's
// `task_to_json` / `event_to_json`, and solexa-events::EventType / solexa-domain::TaskState).
// Kept hand-written rather than codegen'd for Phase 1, a typeshare/specta
// pass to generate these from the Rust types is a reasonable Phase 2 cleanup.

export type TaskState =
  | "created"
  | "planned"
  | "queued"
  | "assigned"
  | "running"
  | "waiting"
  | "blocked"
  | "review"
  | "revision"
  | "verified"
  | "completed"
  | "failed"
  | "cancelled";

export type EventType =
  | "request_received"
  | "planner_started"
  | "planner_completed"
  | "task_created"
  | "task_assigned"
  | "task_accepted"
  | "task_blocked"
  | "context_packaged"
  | "resource_acquired"
  | "resource_released"
  | "model_invoked"
  | "model_completed"
  | "model_token_generated"
  | "result_submitted"
  | "verification_started"
  | "verified"
  | "model_review_started"
  | "model_review_completed"
  | "specialist_review_started"
  | "specialist_review_completed"
  | "specialist_review_skipped"
  | "member_updated"
  | "team_updated"
  | "revision_requested"
  | "escalated"
  | "approved"
  | "rejected"
  | "task_completed"
  | "task_failed"
  | "task_priority_changed"
  | "task_deadline_changed"
  | "knowledge_item_added"
  | "knowledge_item_archived"
  | "decision_recorded"
  | "decision_resolved"
  | "conflict_detected"
  | "conflict_resolved"
  | "member_registered"
  | "team_registered"
  | "team_reparented"
  | "specialist_registered"
  | "team_member_added"
  | "team_member_removed"
  | "specialist_updated"
  | "specialist_active_changed"
  | "specialist_imported"
  | "team_imported"
  | "saved_to_registry"
  | "installed_from_registry"
  | "git_repo_linked"
  | "git_repo_unlinked"
  | "pushed_to_git"
  | "pulled_from_git"
  | "member_deleted"
  | "member_restored"
  | "team_deleted"
  | "team_restored"
  | "require_review_for_incoming_changes_changed"
  | "enforce_approver_roles_changed"
  | "resource_budget_changed"
  | "pending_sync_change_queued"
  | "pending_sync_change_applied"
  | "pending_sync_change_denied"
  | "role_created"
  | "role_updated"
  | "member_role_assigned"
  | "member_role_unassigned"
  | "task_cancelled";

export type ConfidenceLevel = "low" | "medium" | "high";

export interface Confidence {
  knowledge: ConfidenceLevel;
  reasoning: ConfidenceLevel;
  evidence: ConfidenceLevel;
  requirement_coverage: ConfidenceLevel;
  tool_reliability: ConfidenceLevel;
  completeness: ConfidenceLevel;
}

export interface VerificationOutcome {
  method: string;
  passed: boolean;
  details: string;
  verified_at: string;
}

export interface TaskResult {
  content: unknown;
  produced_by_specialist_id: string;
  produced_by_model_id: string;
  tool_calls: string[];
}

export interface TaskDto {
  id: string;
  title: string;
  description: string;
  state: TaskState;
  assigned_specialist_id: string | null;
  required_capabilities: string[];
  priority: number;
  // Round 44: RFC 3339 timestamp, or null when unset.
  deadline: string | null;
  result: TaskResult | null;
  confidence: Confidence | null;
  verification: VerificationOutcome | null;
  created_at: string;
  updated_at: string;
}

export interface EventDto {
  event_id: string;
  timestamp: string;
  actor_id: string;
  actor_type: "human" | "ai" | "system";
  event_type: EventType;
  payload: unknown;
  correlation_id: string;
  causation_id: string | null;
}

export interface SpecialistBenchmarkDto {
  verification_passed_count: number;
  verification_failed_count: number;
  success_rate: number;
}

export type PermissionName = "read" | "propose" | "edit" | "review" | "approve" | "execute";

export interface SpecialistDto {
  id: string;
  name: string;
  role: string;
  domain: string;
  capabilities: string[];
  preferred_model_ids?: string[];
  tools?: string[];
  knowledge_scope?: string[];
  permissions?: PermissionName[];
  output_schema?: unknown;
  escalate_on_weak_confidence?: boolean;
  escalate_on_verification_failure?: boolean;
  requires_model_review?: boolean;
  requires_specialist_review?: boolean;
  active?: boolean;
  benchmark?: SpecialistBenchmarkDto;
  // Round 47: zero or more Role ids this specialist ALSO holds, on top
  // of (never instead of) its own `permissions` above.
  role_ids?: string[];
  // Round 52: optional per-specialist generation limits. null/absent =
  // unset (unbounded / runtime default).
  max_tokens?: number | null;
  temperature?: number | null;
}

export interface ModelDto {
  id: string;
  name: string;
  provider: string;
  parameter_count_billions: number | null;
  local: boolean;
}

// Round 41: what the Models settings panel needs beyond ModelDto, the
// per-project enabled/disabled state, plus a couple of fields (context
// length, tool support, real since round 38's /api/show enrichment)
// worth showing someone deciding whether to keep a model around.
export interface ModelWithStatusDto extends ModelDto {
  context_capacity_tokens: number;
  tool_support: boolean;
  enabled: boolean;
}

export interface CreateSpecialistInput {
  name: string;
  role: string;
  domain: string;
  capabilities: string[];
  preferredModelIds: string[];
  tools: string[];
  knowledgeScope: string[];
  permissions: PermissionName[];
  outputSchema?: string;
  escalateOnWeakConfidence: boolean;
  escalateOnVerificationFailure: boolean;
  requiresModelReview: boolean;
  requiresSpecialistReview: boolean;
  /** Round 52: undefined = unset. Must be a positive integer. */
  maxTokens?: number;
  /** Round 52: undefined = the runtime's default. */
  temperature?: number;
  memberDisplayName: string;
  addToTeamId?: string;
}

export interface CreateSpecialistResult {
  specialist_id: string;
  member_id: string;
}

/** Same structured shape as `CreateSpecialistInput`, minus the
 * member/team fields `update_specialist` doesn't touch, plus the id of
 * the specialist being edited. `active` is deliberately absent, it
 * only changes via a dedicated archive/reactivate call, never as a
 * side effect of an edit. */
export interface UpdateSpecialistInput {
  specialistId: string;
  name: string;
  role: string;
  domain: string;
  capabilities: string[];
  preferredModelIds: string[];
  tools: string[];
  knowledgeScope: string[];
  permissions: PermissionName[];
  outputSchema?: string;
  escalateOnWeakConfidence: boolean;
  escalateOnVerificationFailure: boolean;
  requiresModelReview: boolean;
  requiresSpecialistReview: boolean;
  /** Round 52: undefined CLEARS the limit (an edit replaces, not merges). */
  maxTokens?: number;
  temperature?: number;
}

export interface SubmissionDto {
  task_id: string;
  correlation_id: string;
}

export type MemberType = "human" | "ai" | "hybrid" | "team";

export interface MemberDto {
  id: string;
  display_name: string;
  description: string;
  member_type: MemberType;
  specialist_ids: string[];
  deleted_at: string | null;
  // Round 46: zero or more Role ids this member holds.
  role_ids: string[];
}

export interface TeamDto {
  id: string;
  name: string;
  mission: string;
  member_ids: string[];
  parent_team_id: string | null;
  deleted_at: string | null;
}

export interface UpdateMemberInput {
  memberId: string;
  displayName: string;
  description: string;
  specialistIds: string[];
}

export interface UpdateTeamInput {
  teamId: string;
  name: string;
  mission: string;
}

export interface CreateTeamInput {
  name: string;
  mission: string;
  parentTeamId?: string;
}

export interface CreateTeamResult {
  team_id: string;
}

export interface ImportSpecialistPackageResult {
  specialist_id: string;
}

export interface ImportTeamPackageResult {
  team_id: string;
}

export type RegistryEntryKind = "specialist" | "team";

export interface RegistryEntryDto {
  id: string;
  kind: RegistryEntryKind;
  name: string;
  domain: string;
  capabilities: string[];
  added_at: string;
}

export interface SaveToRegistryResult {
  registry_entry_id: string;
}

export interface InstallFromRegistryResult {
  kind: RegistryEntryKind;
  id: string;
}

export interface ConflictPositionDto {
  specialist_id: string;
  specialist_name: string;
  model_id: string | null;
  content: unknown;
  verification_passed: boolean | null;
}

export type ConflictStatus = "open" | "resolved";

export type SyncObjectKind = "member" | "team" | "specialist" | "knowledge_item" | "decision";

export interface SyncObjectRefDto {
  kind: SyncObjectKind;
  object_id: string;
  field: string | null;
}

export interface ConflictDto {
  id: string;
  task_id: string | null;
  question: string;
  positions: ConflictPositionDto[];
  resolution: string | null;
  approved_by: string | null;
  status: ConflictStatus;
  created_at: string;
  sync_object: SyncObjectRefDto | null;
  github_pr_url: string | null;
}

export interface GitSyncStatusDto {
  repo_path: string;
  linked_at: string;
  last_synced_commit: string | null;
  last_synced_at: string | null;
  local_dirty: boolean;
  // Round 46.
  require_review_for_incoming_changes: boolean;
}

export interface GitPushResultDto {
  pushed: boolean;
  commit: string;
}

export interface GitPullResultDto {
  commit: string;
  had_remote_changes: boolean;
  members_applied: number;
  teams_applied: number;
  specialists_applied: number;
  knowledge_items_applied: number;
  decisions_applied: number;
  conflicts_created: string[];
  // Round 46: ids of PendingSyncChange rows created/refreshed by this
  // pull, only non-empty when require_review_for_incoming_changes is on.
  pending_changes_created: string[];
  github_prs_created: [string, string][];
  github_pr_errors: [string, string][];
}

// Adopted from a parallel build: the Resource Center. `used_*`/`running`/
// `queued` are a point-in-time reading (`Coordinator::resource_snapshot()`);
// `budget_*`/`max_concurrent` are the configured ceiling they're
// measured against. Note `running` can include a task still waiting on
// the VRAM/RAM budget (it also counts in `queued` then), see
// `ResourceManager::acquire`'s doc comment: it's a concurrency-permit
// count, not strictly "actively generating right now."
export interface ResourceSnapshotDto {
  used_vram_mb: number;
  used_ram_mb: number;
  running: number;
  queued: number;
  budget_vram_mb: number;
  budget_ram_mb: number;
  max_concurrent: number;
}

// Round 46: "Discord roles" for SOLEXA, a named, reusable permission
// bundle a Member can hold zero or more of. See
// solexa-domain::role's doc comment for the full rationale.
export interface RoleDto {
  id: string;
  name: string;
  permissions: PermissionName[];
  created_at: string;
}

export type PendingSyncChangeStatus = "pending" | "applied" | "denied";

// The non-conflicting counterpart to ConflictDto, a change
// pull_from_git queued for human review instead of applying
// immediately, because the project has require_review_for_incoming_changes
// set. Deliberately omits the raw object_json/benchmark_json payload:
// the review UI only ever needs kind/label/is_new to decide apply vs.
// deny, not the full incoming object.
export interface PendingSyncChangeDto {
  id: string;
  kind: SyncObjectKind;
  object_id: string;
  label: string;
  is_new: boolean;
  source_commit: string;
  discovered_at: string;
  status: PendingSyncChangeStatus;
  decided_by: string | null;
  decided_at: string | null;
  denial_reason: string | null;
}

export type KnowledgeKind = "fact" | "constraint" | "artifact";
export type KnowledgeStatus =
  | "unverified"
  | "proposed"
  | "accepted"
  | "verified"
  | "rejected"
  | "superseded"
  | "archived";

export interface KnowledgeItemDto {
  id: string;
  kind: KnowledgeKind;
  title: string;
  content: string;
  status: KnowledgeStatus;
  tags: string[];
  created_at: string;
}

export type DecisionStatus = "open" | "resolved";

export interface DecisionDto {
  id: string;
  question: string;
  options: string[];
  resolution: string | null;
  approved_by: string | null;
  status: DecisionStatus;
  tags: string[];
  created_at: string;
  github_issue_url: string | null;
}

// Round 35: multiple projects in one install. The engine was always
// multi-project; this is the shell catching up.
export interface ProjectDto {
  id: string;
  name: string;
  description: string;
  is_active: boolean;
}
