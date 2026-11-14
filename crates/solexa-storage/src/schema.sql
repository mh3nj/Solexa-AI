-- SOLEXA Phase 1 schema.
--
-- This intentionally implements only what the Phase 1 core loop needs
-- (projects, specialists, models, tasks, events). The full spec calls
-- for many more tables (teams, artifacts, decisions, conflicts,
-- evidence, policies, permissions, resource_snapshots, tool_calls,
-- reviews, approvals...), those arrive with the phases that actually
-- use them (Phase 4 project brain, Phase 5 verification/conflict,
-- Phase 6 teams). Creating empty tables for them now would be exactly
-- the "fake placeholder architecture" the spec warns against.
--
-- Nested/structured fields (capabilities, permissions, JSON payloads,
-- etc.) are stored as JSON TEXT columns rather than normalized further:
-- SQLite is the source of truth for authoritative state, but Phase 1
-- doesn't yet need to query inside those structures relationally.

CREATE TABLE IF NOT EXISTS projects (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    -- Round 55: opt-in. 0 (the default, and what every pre-existing project
    -- reads as) = deciding a Decision or resolving a Conflict needs only a
    -- registered Member, exactly as before. 1 = it needs a role granting
    -- Permission::Approve. See migrate()'s add_column_if_missing entry.
    enforce_approver_roles INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS models (
    id                          TEXT PRIMARY KEY,
    name                        TEXT NOT NULL,
    provider                    TEXT NOT NULL,
    modalities_json             TEXT NOT NULL,
    context_capacity_tokens     INTEGER NOT NULL,
    parameter_count_billions    REAL,
    quantization                TEXT,
    estimated_vram_mb           INTEGER NOT NULL,
    estimated_ram_mb            INTEGER NOT NULL,
    tool_support                INTEGER NOT NULL,
    structured_output_support   INTEGER NOT NULL,
    streaming_support           INTEGER NOT NULL,
    local                       INTEGER NOT NULL,
    license                     TEXT
);

CREATE TABLE IF NOT EXISTS specialists (
    id                   TEXT PRIMARY KEY,
    project_id           TEXT NOT NULL REFERENCES projects(id),
    name                 TEXT NOT NULL,
    role                 TEXT NOT NULL,
    domain               TEXT NOT NULL,
    capabilities_json    TEXT NOT NULL,
    preferred_model_ids_json TEXT NOT NULL,
    tools_json           TEXT NOT NULL,
    knowledge_scope_json TEXT NOT NULL,
    permissions_json     TEXT NOT NULL,
    output_schema_json   TEXT,
    escalation_policy_json TEXT NOT NULL,
    active                INTEGER NOT NULL DEFAULT 1,
    -- Round 47: see migrate()'s add_column_if_missing entry for a
    -- database created before this column existed.
    role_ids_json         TEXT NOT NULL DEFAULT '[]',
    -- Round 52: optional per-specialist generation limits; NULL = unset
    -- (unbounded / runtime default). See migrate()'s add_column_if_missing.
    max_tokens            INTEGER,
    temperature           REAL
);

CREATE TABLE IF NOT EXISTS tasks (
    id                        TEXT PRIMARY KEY,
    project_id                TEXT NOT NULL REFERENCES projects(id),
    title                     TEXT NOT NULL,
    description               TEXT NOT NULL,
    state                     TEXT NOT NULL,
    owner_id                  TEXT,
    assigned_specialist_id    TEXT,
    parent_task_id            TEXT,
    required_capabilities_json TEXT NOT NULL,
    priority                  INTEGER NOT NULL,
    context_package_json      TEXT,
    result_json               TEXT,
    confidence_json           TEXT,
    verification_json         TEXT,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL,
    -- Round 44: optional target completion time, NULL by default.
    -- Also see migrate()'s add_column_if_missing calls for both this
    -- and `priority`, a database created before this table definition
    -- included one of these columns needs the ALTER TABLE path, since
    -- CREATE TABLE IF NOT EXISTS is a no-op once the table exists.
    deadline                  TEXT
);

CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);

CREATE TABLE IF NOT EXISTS events (
    event_id       TEXT PRIMARY KEY,
    project_id     TEXT NOT NULL,
    timestamp      TEXT NOT NULL,
    actor_id       TEXT NOT NULL,
    actor_type     TEXT NOT NULL,
    event_type     TEXT NOT NULL,
    payload_json   TEXT NOT NULL,
    correlation_id TEXT NOT NULL,
    causation_id   TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_correlation ON events(correlation_id);
CREATE INDEX IF NOT EXISTS idx_events_project ON events(project_id);

-- Phase 4: the Project Brain. `knowledge_items` covers facts,
-- constraints, and artifacts with one shared shape (see
-- solexa-domain::knowledge for why); `decisions` gets its own table
-- because its shape (options, resolution, approver) doesn't fit the
-- same columns. Both carry provenance and a status, per the spec rule
-- that uncertain AI output must never silently become permanent truth.

CREATE TABLE IF NOT EXISTS knowledge_items (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id),
    kind             TEXT NOT NULL,
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    status           TEXT NOT NULL,
    provenance_json  TEXT NOT NULL,
    tags_json        TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_knowledge_items_project ON knowledge_items(project_id);

CREATE TABLE IF NOT EXISTS decisions (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL REFERENCES projects(id),
    question      TEXT NOT NULL,
    options_json  TEXT NOT NULL,
    resolution    TEXT,
    approved_by   TEXT,
    status        TEXT NOT NULL,
    tags_json     TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    github_issue_url TEXT
);

CREATE INDEX IF NOT EXISTS idx_decisions_project ON decisions(project_id);

-- Phase 5: preserved disagreement. `positions_json` holds every
-- specialist's differing result (see solexa-domain::conflict), never
-- just the "winning" one, even after resolution. `task_id` is an empty
-- string (never NULL) for a round-22 sync conflict, which has no task
-- see solexa-domain::conflict for why a sentinel was chosen over a
-- nullable column here specifically. `sync_object_kind`/`sync_object_id`
-- are nullable and only set for sync conflicts; `sync_object_field`
-- (round 25) is nullable within that, only unset for a sync conflict
-- created before round 25's field-level granularity.
CREATE TABLE IF NOT EXISTS conflicts (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects(id),
    task_id           TEXT NOT NULL,
    question          TEXT NOT NULL,
    positions_json    TEXT NOT NULL,
    resolution        TEXT,
    approved_by       TEXT,
    status            TEXT NOT NULL,
    created_at        TEXT NOT NULL,
    sync_object_kind  TEXT,
    sync_object_id    TEXT,
    sync_object_field TEXT,
    github_pr_url     TEXT
);

CREATE INDEX IF NOT EXISTS idx_conflicts_project ON conflicts(project_id);
CREATE INDEX IF NOT EXISTS idx_conflicts_task ON conflicts(task_id);

-- Phase 6: Member/Team layer. `member_type` distinguishes human/ai/
-- hybrid/team members (spec: "MODEL != MEMBER", a member is who's
-- accountable, a Specialist is what capability config an AI member
-- runs). `parent_team_id` on teams expresses nesting by reference
-- rather than inline containment (see solexa-domain::team).

CREATE TABLE IF NOT EXISTS members (
    id                 TEXT PRIMARY KEY,
    project_id         TEXT NOT NULL REFERENCES projects(id),
    display_name       TEXT NOT NULL,
    description        TEXT NOT NULL,
    member_type        TEXT NOT NULL,
    specialist_ids_json TEXT NOT NULL,
    deleted_at         TEXT,
    -- Round 46: see migrate()'s add_column_if_missing entry for a
    -- database created before this column existed.
    role_ids_json      TEXT NOT NULL DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_members_project ON members(project_id);

-- Round 46: a named, reusable permission bundle (Read/Propose/Edit/
-- Review/Approve/Execute, the same `Permission` enum Specialist has
-- carried since Phase 3) a Member can hold zero or more of. See
-- solexa-domain::role for the full "Discord roles" rationale.
CREATE TABLE IF NOT EXISTS roles (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id),
    name             TEXT NOT NULL,
    permissions_json TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_roles_project ON roles(project_id);

CREATE TABLE IF NOT EXISTS teams (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id),
    name             TEXT NOT NULL,
    mission          TEXT NOT NULL,
    member_ids_json  TEXT NOT NULL,
    parent_team_id   TEXT,
    deleted_at       TEXT
);

CREATE INDEX IF NOT EXISTS idx_teams_project ON teams(project_id);
CREATE INDEX IF NOT EXISTS idx_teams_parent ON teams(parent_team_id);

-- Phase 8: measured, historical performance per specialist (see
-- solexa-domain::benchmark). One row per specialist, incremented on
-- every real attempt rather than replaced, this is a running tally,
-- not a point-in-time score.
CREATE TABLE IF NOT EXISTS specialist_benchmarks (
    specialist_id              TEXT PRIMARY KEY,
    verification_passed_count  INTEGER NOT NULL DEFAULT 0,
    verification_failed_count  INTEGER NOT NULL DEFAULT 0
);

-- The local registry (spec's "Specialist Discovery"/"Specialist
-- Marketplace" sections, local-first slice): a personal library of
-- saved specialist/team packages, independent of any one project -
-- there is deliberately no project_id here. See
-- solexa-domain::registry for the full rationale, including why
-- removing an entry is a genuine delete rather than following the
-- archive-only rule live Specialists/Members/Teams hold.
CREATE TABLE IF NOT EXISTS registry_entries (
    id                 TEXT PRIMARY KEY,
    kind               TEXT NOT NULL,
    name               TEXT NOT NULL,
    domain             TEXT NOT NULL,
    capabilities_json  TEXT NOT NULL,
    package_json       TEXT NOT NULL,
    added_at           TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_registry_entries_kind ON registry_entries(kind);

-- Phase 8 (roadmap): a project's link to a local git working directory
-- used as its collaboration "server", see solexa-domain::sync for the
-- full rationale (no SOLEXA server, no SOLEXA accounts; the user's own
-- git credentials and the remote's own collaborator system do all the
-- work). One row per project (v1), project_id is the primary key.
CREATE TABLE IF NOT EXISTS git_sync_configs (
    project_id                  TEXT PRIMARY KEY REFERENCES projects(id),
    repo_path                   TEXT NOT NULL,
    linked_at                   TEXT NOT NULL,
    last_synced_commit          TEXT,
    last_synced_snapshot_json   TEXT,
    last_synced_at              TEXT,
    -- Round 46: see migrate()'s add_column_if_missing entry for a
    -- database created before this column existed.
    require_review_for_incoming_changes INTEGER NOT NULL DEFAULT 0
);

-- Round 46: a non-conflicting incoming change from pull_from_git, held
-- for a human to explicitly apply or deny rather than fast-forwarded
-- automatically, only ever created for a project whose git_sync_config
-- has require_review_for_incoming_changes set. See solexa-domain::sync
-- for the full rationale and PendingSyncChange's own doc comment.
CREATE TABLE IF NOT EXISTS pending_sync_changes (
    id               TEXT PRIMARY KEY,
    project_id       TEXT NOT NULL REFERENCES projects(id),
    kind             TEXT NOT NULL,
    object_id        TEXT NOT NULL,
    label            TEXT NOT NULL,
    object_json      TEXT NOT NULL,
    benchmark_json   TEXT,
    is_new           INTEGER NOT NULL,
    source_commit    TEXT NOT NULL,
    discovered_at    TEXT NOT NULL,
    status           TEXT NOT NULL,
    decided_by       TEXT,
    decided_at       TEXT,
    denial_reason    TEXT
);

CREATE INDEX IF NOT EXISTS idx_pending_sync_changes_project ON pending_sync_changes(project_id);
-- Round 46's dedup lookup: "is there already a Pending row for this
-- exact object" runs on every pull, once per to_apply item.
CREATE INDEX IF NOT EXISTS idx_pending_sync_changes_object ON pending_sync_changes(project_id, kind, object_id, status);

-- Round 41: per-project model configuration. `models` (above) stays a
-- single registry shared across every project in this install, models
-- are reported per-daemon (round 37's OllamaProvider, or
-- MockLocalProvider), not per-project, so there's nothing project-
-- specific to store on that table itself. A row here means the model
-- is *disabled for this project specifically*; no row means enabled.
-- That default has to be "everything available", flipping it to
-- opt-in would silently break every project and test that predates
-- this table and never configured anything.
CREATE TABLE IF NOT EXISTS project_disabled_models (
    project_id   TEXT NOT NULL REFERENCES projects(id),
    model_id     TEXT NOT NULL,
    disabled_at  TEXT NOT NULL,
    PRIMARY KEY (project_id, model_id)
);

-- Round 61: app-level (not per-project) settings as plain key/value text.
-- First user: the engine's resource budget, resized live from the Resource
-- Center (round 60) and now remembered across restarts. A brand-new table, so
-- `CREATE TABLE IF NOT EXISTS` is the whole migration. The caller owns the
-- meaning and validation of each value, this table stores text, nothing more.
CREATE TABLE IF NOT EXISTS app_settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);
