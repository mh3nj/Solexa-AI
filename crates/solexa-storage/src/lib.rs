//! solexa-storage, SQLite is the authoritative source of local project
//! state (spec rule: "Do not store authoritative project state only
//! inside embeddings" / "Do not make SQLite temporary by default").
//!
//! `Storage` owns a single `rusqlite::Connection` behind a mutex. That's
//! a deliberate Phase 1 simplification: one desktop process, modest
//! throughput, correctness over concurrency. If a later phase's
//! scheduler needs concurrent writers, swap this for `spawn_blocking` or
//! an async driver, the public API here is already synchronous-inside,
//! so callers wouldn't need to change.
//!
//! This crate also implements `solexa_events::EventSink`, so the event
//! bus can persist directly into the `events` table without the events
//! crate ever knowing SQLite exists.

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension, Row};
use solexa_domain::{
    BenchmarkOutcome, Confidence, Conflict, ConflictStatus, ContextPackage, Decision,
    DecisionStatus, EscalationPolicy, GitSyncConfig, KnowledgeItem, KnowledgeKind,
    KnowledgeStatus, Member, MemberType, ModelMetadata, PendingSyncChange, PendingSyncChangeStatus,
    Permission, ProjectSummary, Provenance,
    RegistryEntry, RegistryEntryKind, Role, Specialist, SpecialistBenchmark, SyncObjectKind,
    SyncObjectRef, Task, TaskResult, TaskState, Team, Uuid, VerificationOutcome,
};
use solexa_events::{ActorType, Event, EventSink, EventType};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = include_str!("schema.sql");

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening SQLite database at {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)
            .context("applying SOLEXA schema")?;
        Self::migrate(&conn).context("applying SOLEXA schema migrations")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Schema changes that can't be expressed as `CREATE TABLE IF NOT
    /// EXISTS`, because they add a column to a table that may already
    /// exist from a database created by an earlier round. SQLite has
    /// no `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`, so each migration
    /// checks `PRAGMA table_info` itself before running. Round 22 is
    /// the first time this codebase has needed to alter an existing
    /// table rather than add a new one, every schema change before it
    /// was a brand new `CREATE TABLE IF NOT EXISTS`.
    fn migrate(conn: &Connection) -> Result<()> {
        Self::add_column_if_missing(conn, "conflicts", "sync_object_kind", "TEXT")?;
        Self::add_column_if_missing(conn, "conflicts", "sync_object_id", "TEXT")?;
        Self::add_column_if_missing(conn, "conflicts", "sync_object_field", "TEXT")?;
        Self::add_column_if_missing(conn, "members", "deleted_at", "TEXT")?;
        Self::add_column_if_missing(conn, "teams", "deleted_at", "TEXT")?;
        Self::add_column_if_missing(conn, "conflicts", "github_pr_url", "TEXT")?;
        Self::add_column_if_missing(conn, "decisions", "github_issue_url", "TEXT")?;
        // Round 44: `priority` (round 36) went straight into
        // schema.sql's CREATE TABLE IF NOT EXISTS without a migration
        // entry, harmless for every test here (`Storage::in_memory`
        // always starts fresh) but a real gap for an actual desktop
        // install's persisted database file predating round 36, which
        // this retroactively fixes. `deadline` is this round's own new
        // column, added the correct way from the start.
        Self::add_column_if_missing(conn, "tasks", "priority", "INTEGER NOT NULL DEFAULT 5")?;
        Self::add_column_if_missing(conn, "tasks", "deadline", "TEXT")?;
        // Round 46: see schema.sql's own comment on this column.
        Self::add_column_if_missing(conn, "git_sync_configs", "require_review_for_incoming_changes", "INTEGER NOT NULL DEFAULT 0")?;
        Self::add_column_if_missing(conn, "members", "role_ids_json", "TEXT NOT NULL DEFAULT '[]'")?;
        Self::add_column_if_missing(conn, "specialists", "role_ids_json", "TEXT NOT NULL DEFAULT '[]'")?;
        // Round 55: NOT NULL DEFAULT 0, so every pre-existing project reads
        // as "not enforced", the opt-in is genuinely default-off.
        Self::add_column_if_missing(conn, "projects", "enforce_approver_roles", "INTEGER NOT NULL DEFAULT 0")?;
        // Round 52: nullable, so an old row simply reads as "unset".
        Self::add_column_if_missing(conn, "specialists", "max_tokens", "INTEGER")?;
        Self::add_column_if_missing(conn, "specialists", "temperature", "REAL")?;
        // Round 64: data heal, not a schema change. Until this round the
        // coordinator stored the assigned *specialist's* id in
        // `tasks.owner_id`, a column documented (and meant) to hold a *member*
        // id. UUIDs do not collide, so an owner_id that equals some specialist's
        // id is provably that bug's output, clear it. Real member owners are
        // untouched; the task's `assigned_specialist_id` (which was correct)
        // is untouched. Idempotent, and a no-op on a clean database.
        conn.execute(
            "UPDATE tasks SET owner_id = NULL WHERE owner_id IN (SELECT id FROM specialists)",
            [],
        )?;
        Ok(())
    }

    fn add_column_if_missing(conn: &Connection, table: &str, column: &str, sql_type: &str) -> Result<()> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !existing.iter().any(|c| c == column) {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {sql_type}"), [])?;
        }
        Ok(())
    }

    // ---- app settings (round 61) -----------------------------------

    /// An app-level setting, or `None` if it was never set.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |row| row.get::<_, String>(0))
            .optional()?)
    }

    /// Removes an app-level setting. A no-op (not an error) if it was never set.
    pub fn delete_setting(&self, key: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM app_settings WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// **Test hook, never call this outside tests.** Drops the settings table
    /// so the next `set_setting`/`get_setting` fails, letting a test in another
    /// crate exercise the "saving failed" path that nothing else can reach
    /// (`cfg(test)` does not cross crate boundaries).
    #[doc(hidden)]
    pub fn sabotage_settings_table_for_tests(&self) {
        let conn = self.conn.lock().unwrap();
        conn.execute("DROP TABLE app_settings", []).unwrap();
    }

    /// Sets (or replaces) an app-level setting.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- projects ---------------------------------------------------

    pub fn insert_project(&self, id: Uuid, name: &str, description: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                id.to_string(),
                name,
                description,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Round 55: whether this project makes deciding a Decision / resolving
    /// a Conflict require `Permission::Approve`. Errors for an unknown
    /// project rather than quietly answering `false`, a caller asking about
    /// a project that doesn't exist has a bug worth surfacing.
    pub fn get_enforce_approver_roles(&self, project_id: Uuid) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT enforce_approver_roles FROM projects WHERE id = ?1",
            params![project_id.to_string()],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("project {project_id} not found"))
    }

    /// Round 55: see `get_enforce_approver_roles`.
    pub fn set_enforce_approver_roles(&self, project_id: Uuid, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE projects SET enforce_approver_roles = ?2 WHERE id = ?1",
            params![project_id.to_string(), enabled],
        )?;
        if changed == 0 {
            return Err(anyhow::anyhow!("project {project_id} not found"));
        }
        Ok(())
    }

    /// Round 35: every project in the database, newest first. The
    /// engine has always been multi-project (every storage and
    /// coordinator function takes a `project_id`); this is the piece
    /// that was missing to let the desktop app actually show more than
    /// the one project `seed.rs` creates at first launch.
    pub fn list_projects(&self) -> Result<Vec<ProjectSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, name, description FROM projects ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProjectSummary {
                    id: Uuid::parse_str(&row.get::<_, String>(0)?).expect("stored uuid is well-formed"),
                    name: row.get(1)?,
                    description: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Genuinely missing until round 22, see `ProjectSummary`'s doc
    /// comment for why nothing needed this before `export_project_snapshot`.
    pub fn get_project(&self, id: Uuid) -> Result<Option<ProjectSummary>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, description FROM projects WHERE id = ?1",
            params![id.to_string()],
            |row| {
                Ok(ProjectSummary {
                    id: Uuid::parse_str(&row.get::<_, String>(0)?).expect("stored uuid is well-formed"),
                    name: row.get(1)?,
                    description: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    // ---- models -------------------------------------------------------

    pub fn insert_model(&self, model: &ModelMetadata) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO models (id, name, provider, modalities_json, context_capacity_tokens,
                parameter_count_billions, quantization, estimated_vram_mb, estimated_ram_mb,
                tool_support, structured_output_support, streaming_support, local, license)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                model.id.to_string(),
                model.name,
                model.provider,
                serde_json::to_string(&model.modalities)?,
                model.context_capacity_tokens,
                model.parameter_count_billions,
                model.quantization,
                model.estimated_vram_mb,
                model.estimated_ram_mb,
                model.tool_support,
                model.structured_output_support,
                model.streaming_support,
                model.local,
                model.license,
            ],
        )?;
        Ok(())
    }

    pub fn list_models(&self) -> Result<Vec<ModelMetadata>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM models ORDER BY rowid")?;
        let rows = stmt
            .query_map([], model_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_model(&self, id: Uuid) -> Result<Option<ModelMetadata>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM models WHERE id = ?1")?;
        Ok(stmt
            .query_row(params![id.to_string()], model_from_row)
            .optional()?)
    }

    /// Round 41: which models `project_id` has explicitly turned off.
    /// Everything *not* in this list is enabled, see
    /// `project_disabled_models`'s own comment in schema.sql for why
    /// the default has to be "everything available" rather than
    /// "nothing available until configured."
    pub fn disabled_model_ids_for_project(&self, project_id: Uuid) -> Result<Vec<Uuid>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT model_id FROM project_disabled_models WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().map(|s| Uuid::parse_str(&s).expect("stored uuid is well-formed")).collect())
    }

    /// Disables or re-enables `model_id` for `project_id` specifically,
    /// every other project sharing the same underlying provider/daemon
    /// is unaffected. Idempotent either way: disabling an
    /// already-disabled model, or enabling an already-enabled one, is
    /// not an error.
    pub fn set_model_enabled_for_project(&self, project_id: Uuid, model_id: Uuid, enabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        if enabled {
            conn.execute(
                "DELETE FROM project_disabled_models WHERE project_id = ?1 AND model_id = ?2",
                params![project_id.to_string(), model_id.to_string()],
            )?;
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO project_disabled_models (project_id, model_id, disabled_at) VALUES (?1, ?2, ?3)",
                params![project_id.to_string(), model_id.to_string(), chrono::Utc::now().to_rfc3339()],
            )?;
        }
        Ok(())
    }

    // ---- specialists ----------------------------------------------------

    pub fn insert_specialist(&self, project_id: Uuid, specialist: &Specialist) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO specialists (id, project_id, name, role, domain, capabilities_json,
                preferred_model_ids_json, tools_json, knowledge_scope_json, permissions_json,
                output_schema_json, escalation_policy_json, active, role_ids_json,
                max_tokens, temperature)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            params![
                specialist.id.to_string(),
                project_id.to_string(),
                specialist.name,
                specialist.role,
                specialist.domain,
                serde_json::to_string(&specialist.capabilities)?,
                serde_json::to_string(&specialist.preferred_model_ids)?,
                serde_json::to_string(&specialist.tools)?,
                serde_json::to_string(&specialist.knowledge_scope)?,
                serde_json::to_string(&specialist.permissions)?,
                specialist
                    .output_schema
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&specialist.escalation_policy)?,
                specialist.active,
                serde_json::to_string(&specialist.role_ids)?,
                specialist.max_tokens,
                specialist.temperature,
            ],
        )?;
        Ok(())
    }

    /// Rewrites a specialist's stored row in place, used by both
    /// editing a specialist's configuration and flipping `active`
    /// (archiving/reactivating). `project_id` isn't part of the update:
    /// a specialist doesn't change projects, so the column is simply
    /// left untouched. `role_ids` (round 47) IS part of the update,
    /// set by `Coordinator::assign_role_to_specialist`/
    /// `unassign_role_from_specialist`, same as `Member::role_ids`.
    pub fn update_specialist(&self, specialist: &Specialist) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE specialists SET name=?2, role=?3, domain=?4, capabilities_json=?5,
                preferred_model_ids_json=?6, tools_json=?7, knowledge_scope_json=?8,
                permissions_json=?9, output_schema_json=?10, escalation_policy_json=?11, active=?12,
                role_ids_json=?13, max_tokens=?14, temperature=?15
             WHERE id = ?1",
            params![
                specialist.id.to_string(),
                specialist.name,
                specialist.role,
                specialist.domain,
                serde_json::to_string(&specialist.capabilities)?,
                serde_json::to_string(&specialist.preferred_model_ids)?,
                serde_json::to_string(&specialist.tools)?,
                serde_json::to_string(&specialist.knowledge_scope)?,
                serde_json::to_string(&specialist.permissions)?,
                specialist
                    .output_schema
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&specialist.escalation_policy)?,
                specialist.active,
                serde_json::to_string(&specialist.role_ids)?,
                specialist.max_tokens,
                specialist.temperature,
            ],
        )?;
        Ok(())
    }

    pub fn get_specialist(&self, id: Uuid) -> Result<Option<Specialist>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM specialists WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], specialist_from_row).optional()?)
    }

    /// Specialists in **creation order** (insertion order, `ORDER BY rowid`),
    /// stable across edits. Round 65: this order is load-bearing, when several
    /// specialists tie on track record, the first one listed becomes the
    /// task's `primary` (owner-of-record). Every list query in this file that
    /// has no more meaningful sort key orders by `rowid` for the same reason:
    /// insertion order, explicitly, rather than whatever plan SQLite picks.
    pub fn list_specialists(&self, project_id: Uuid) -> Result<Vec<Specialist>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM specialists WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], specialist_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- tasks ----------------------------------------------------------

    pub fn insert_task(&self, task: &Task) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (id, project_id, title, description, state, owner_id,
                assigned_specialist_id, parent_task_id, required_capabilities_json, priority,
                context_package_json, result_json, confidence_json, verification_json,
                created_at, updated_at, deadline)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params_from_iter(task_params(task)?),
        )?;
        Ok(())
    }

    pub fn update_task(&self, task: &Task) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE tasks SET project_id=?2, title=?3, description=?4, state=?5, owner_id=?6,
                assigned_specialist_id=?7, parent_task_id=?8, required_capabilities_json=?9,
                priority=?10, context_package_json=?11, result_json=?12, confidence_json=?13,
                verification_json=?14, created_at=?15, updated_at=?16, deadline=?17
             WHERE id = ?1",
            params_from_iter(task_params(task)?),
        )?;
        Ok(())
    }

    /// Round 57: changes ONLY a task's `priority`. `update_task` rewrites
    /// the whole row, so a caller that read the task, changed one field and
    /// wrote it back could overwrite a `state`/`result` the background
    /// pipeline advanced in between (a lost update, the same class as the
    /// cancelled-task resurrection round 53 closed). A single-column
    /// `UPDATE` has no such window. Returns whether the task exists.
    pub fn set_task_priority(&self, id: Uuid, priority: u8) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE tasks SET priority = ?2 WHERE id = ?1",
            params![id.to_string(), priority as i64],
        )?;
        Ok(changed > 0)
    }

    /// Round 57: changes ONLY a task's `deadline`, see `set_task_priority`.
    pub fn set_task_deadline(&self, id: Uuid, deadline: Option<chrono::DateTime<chrono::Utc>>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE tasks SET deadline = ?2 WHERE id = ?1",
            params![id.to_string(), deadline.map(|d| d.to_rfc3339())],
        )?;
        Ok(changed > 0)
    }

    pub fn get_task(&self, id: Uuid) -> Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM tasks WHERE id = ?1")?;
        Ok(stmt
            .query_row(params![id.to_string()], task_from_row)
            .optional()?)
    }

    pub fn list_tasks(&self, project_id: Uuid) -> Result<Vec<Task>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM tasks WHERE project_id = ?1 ORDER BY created_at")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], task_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- events -----------------------------------------------------------

    pub fn list_events_for_correlation(&self, correlation_id: Uuid) -> Result<Vec<Event>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM events WHERE correlation_id = ?1 ORDER BY timestamp",
        )?;
        let rows = stmt
            .query_map(params![correlation_id.to_string()], event_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- knowledge items (Phase 4: Project Brain) -----------------------

    pub fn insert_knowledge_item(&self, item: &KnowledgeItem) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO knowledge_items (id, project_id, kind, title, content, status,
                provenance_json, tags_json, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                item.id.to_string(),
                item.project_id.to_string(),
                serde_json::to_string(&item.kind)?,
                item.title,
                item.content,
                serde_json::to_string(&item.status)?,
                serde_json::to_string(&item.provenance)?,
                serde_json::to_string(&item.tags)?,
                item.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_knowledge_items(&self, project_id: Uuid) -> Result<Vec<KnowledgeItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM knowledge_items WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], knowledge_item_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_knowledge_item(&self, id: Uuid) -> Result<Option<KnowledgeItem>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM knowledge_items WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], knowledge_item_from_row).optional()?)
    }

    /// Changes only a knowledge item's status, never deletes the row.
    /// Spec rule: uncertain or outdated project knowledge should be
    /// marked Rejected/Superseded/Archived, not erased; the provenance
    /// and history stay queryable even once something is retired.
    pub fn update_knowledge_item_status(&self, id: Uuid, status: KnowledgeStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE knowledge_items SET status = ?2 WHERE id = ?1",
            params![id.to_string(), serde_json::to_string(&status)?],
        )?;
        Ok(())
    }

    // ---- decisions (Phase 4: Project Brain) ------------------------------

    pub fn insert_decision(&self, decision: &Decision) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO decisions (id, project_id, question, options_json, resolution,
                approved_by, status, tags_json, created_at, github_issue_url)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                decision.id.to_string(),
                decision.project_id.to_string(),
                decision.question,
                serde_json::to_string(&decision.options)?,
                decision.resolution,
                decision.approved_by,
                serde_json::to_string(&decision.status)?,
                serde_json::to_string(&decision.tags)?,
                decision.created_at.to_rfc3339(),
                decision.github_issue_url,
            ],
        )?;
        Ok(())
    }

    pub fn list_decisions(&self, project_id: Uuid) -> Result<Vec<Decision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM decisions WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], decision_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_decision(&self, id: Uuid) -> Result<Option<Decision>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM decisions WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], decision_from_row).optional()?)
    }

    pub fn update_decision(&self, decision: &Decision) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE decisions SET resolution=?2, approved_by=?3, status=?4 WHERE id = ?1",
            params![
                decision.id.to_string(),
                decision.resolution,
                decision.approved_by,
                serde_json::to_string(&decision.status)?,
            ],
        )?;
        Ok(())
    }

    /// Round 33: records the GitHub Issue opened to mirror a decision,
    /// separate from `update_decision` since it happens at a different
    /// moment (right after `send_decision_to_github`, well before
    /// anyone resolves anything) and never touches resolution state.
    pub fn set_decision_github_issue_url(&self, decision_id: Uuid, url: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE decisions SET github_issue_url=?2 WHERE id = ?1",
            params![decision_id.to_string(), url],
        )?;
        Ok(())
    }

    // ---- conflicts (Phase 5) ---------------------------------------------

    pub fn insert_conflict(&self, conflict: &Conflict) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let (sync_kind, sync_id, sync_field) = match &conflict.sync_object {
            Some(r) => (Some(serde_json::to_string(&r.kind)?), Some(r.object_id.to_string()), r.field.clone()),
            None => (None, None, None),
        };
        conn.execute(
            "INSERT INTO conflicts (id, project_id, task_id, question, positions_json,
                resolution, approved_by, status, created_at, sync_object_kind, sync_object_id, sync_object_field, github_pr_url)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                conflict.id.to_string(),
                conflict.project_id.to_string(),
                conflict.task_id.map(|t| t.to_string()).unwrap_or_default(),
                conflict.question,
                serde_json::to_string(&conflict.positions)?,
                conflict.resolution,
                conflict.approved_by,
                serde_json::to_string(&conflict.status)?,
                conflict.created_at.to_rfc3339(),
                sync_kind,
                sync_id,
                sync_field,
                conflict.github_pr_url,
            ],
        )?;
        Ok(())
    }

    pub fn update_conflict(&self, conflict: &Conflict) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conflicts SET resolution=?2, approved_by=?3, status=?4 WHERE id = ?1",
            params![
                conflict.id.to_string(),
                conflict.resolution,
                conflict.approved_by,
                serde_json::to_string(&conflict.status)?,
            ],
        )?;
        Ok(())
    }

    /// Round 30: records the GitHub PR opened for a conflict, separate
    /// from `update_conflict` since it happens at a different moment
    /// (right after `send_conflict_to_github` creates the PR, well
    /// before anyone resolves anything) and never touches
    /// resolution/approval state.
    pub fn set_conflict_github_pr_url(&self, conflict_id: Uuid, url: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE conflicts SET github_pr_url=?2 WHERE id = ?1",
            params![conflict_id.to_string(), url],
        )?;
        Ok(())
    }

    pub fn get_conflict(&self, id: Uuid) -> Result<Option<Conflict>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM conflicts WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], conflict_from_row).optional()?)
    }

    pub fn list_conflicts(&self, project_id: Uuid) -> Result<Vec<Conflict>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM conflicts WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], conflict_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- members (Phase 6) -----------------------------------------------

    pub fn insert_member(&self, member: &Member) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO members (id, project_id, display_name, description, member_type, specialist_ids_json, deleted_at, role_ids_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                member.id.to_string(),
                member.project_id.to_string(),
                member.display_name,
                member.description,
                serde_json::to_string(&member.member_type)?,
                serde_json::to_string(&member.specialist_ids)?,
                member.deleted_at.map(|t| t.to_rfc3339()),
                serde_json::to_string(&member.role_ids)?,
            ],
        )?;
        Ok(())
    }

    pub fn get_member(&self, id: Uuid) -> Result<Option<Member>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM members WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], member_from_row).optional()?)
    }

    /// Rewrites a member's stored row in place. `project_id` and
    /// `member_type` aren't part of the update, a member doesn't
    /// change projects, and identity (human vs. AI vs. hybrid) isn't
    /// something an edit changes; `Coordinator::edit_member` enforces
    /// that by overriding both from the existing row before calling
    /// this, same defensive shape `edit_specialist` uses for `active`.
    /// `deleted_at` *is* part of the update (round 27), set by
    /// `delete_member`/`restore_member`, and also the field a sync
    /// pull fast-forwards or merges like any other. `role_ids` is also
    /// part of the update (round 46), set by
    /// `Coordinator::assign_role`/`unassign_role`.
    pub fn update_member(&self, member: &Member) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE members SET display_name=?2, description=?3, specialist_ids_json=?4, deleted_at=?5, role_ids_json=?6
             WHERE id = ?1",
            params![
                member.id.to_string(),
                member.display_name,
                member.description,
                serde_json::to_string(&member.specialist_ids)?,
                member.deleted_at.map(|t| t.to_rfc3339()),
                serde_json::to_string(&member.role_ids)?,
            ],
        )?;
        Ok(())
    }

    pub fn list_members(&self, project_id: Uuid) -> Result<Vec<Member>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM members WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], member_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- roles (round 46) ---------------------------------------------

    pub fn insert_role(&self, role: &Role) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO roles (id, project_id, name, permissions_json, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![
                role.id.to_string(),
                role.project_id.to_string(),
                role.name,
                serde_json::to_string(&role.permissions)?,
                role.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Renames a role or changes its permission set, every member
    /// holding it picks up the change immediately (no per-member copy
    /// to update), the entire point of a reusable role over an ad-hoc
    /// per-member list.
    pub fn update_role(&self, role: &Role) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE roles SET name=?2, permissions_json=?3 WHERE id = ?1",
            params![role.id.to_string(), role.name, serde_json::to_string(&role.permissions)?],
        )?;
        Ok(())
    }

    pub fn get_role(&self, id: Uuid) -> Result<Option<Role>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM roles WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], role_from_row).optional()?)
    }

    pub fn list_roles(&self, project_id: Uuid) -> Result<Vec<Role>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM roles WHERE project_id = ?1 ORDER BY created_at ASC")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], role_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- teams (Phase 6) ---------------------------------------------------

    pub fn insert_team(&self, team: &Team) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO teams (id, project_id, name, mission, member_ids_json, parent_team_id, deleted_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                team.id.to_string(),
                team.project_id.to_string(),
                team.name,
                team.mission,
                serde_json::to_string(&team.member_ids)?,
                team.parent_team_id.map(|id| id.to_string()),
                team.deleted_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Rewrites a team's stored row in place, needed the moment a team's
    /// membership can change after creation (e.g. a newly created
    /// specialist's AI member joining the default team), not just at
    /// creation time via `insert_team`. `deleted_at` is part of the
    /// update (round 27) for the same reason as `update_member`'s.
    pub fn update_team(&self, team: &Team) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE teams SET project_id=?2, name=?3, mission=?4, member_ids_json=?5, parent_team_id=?6, deleted_at=?7
             WHERE id = ?1",
            params![
                team.id.to_string(),
                team.project_id.to_string(),
                team.name,
                team.mission,
                serde_json::to_string(&team.member_ids)?,
                team.parent_team_id.map(|id| id.to_string()),
                team.deleted_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn get_team(&self, id: Uuid) -> Result<Option<Team>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM teams WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], team_from_row).optional()?)
    }

    pub fn list_teams(&self, project_id: Uuid) -> Result<Vec<Team>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM teams WHERE project_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], team_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Direct sub-teams of `parent_team_id`, the building block
    /// `solexa-coordinator::team_members_recursive` walks to flatten a
    /// nested hierarchy.
    pub fn list_child_teams(&self, parent_team_id: Uuid) -> Result<Vec<Team>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM teams WHERE parent_team_id = ?1 ORDER BY rowid")?;
        let rows = stmt
            .query_map(params![parent_team_id.to_string()], team_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- specialist benchmarks (Phase 8) -----------------------------------

    /// Increments the running tally for one specialist by one outcome.
    /// An upsert rather than an insert-then-update: the first attempt
    /// for a specialist creates its row, every attempt after that just
    /// adds to the existing counts.
    pub fn record_benchmark_outcome(&self, specialist_id: Uuid, outcome: BenchmarkOutcome) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let (passed_delta, failed_delta) = match outcome {
            BenchmarkOutcome::VerificationPassed => (1, 0),
            BenchmarkOutcome::VerificationFailed => (0, 1),
        };
        conn.execute(
            "INSERT INTO specialist_benchmarks (specialist_id, verification_passed_count, verification_failed_count)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(specialist_id) DO UPDATE SET
                verification_passed_count = verification_passed_count + ?2,
                verification_failed_count = verification_failed_count + ?3",
            params![specialist_id.to_string(), passed_delta, failed_delta],
        )?;
        Ok(())
    }

    /// Absolute upsert, replaces a specialist's benchmark counts
    /// outright rather than accumulating a delta the way
    /// `record_benchmark_outcome` does. Used by specialist package
    /// import to seed a newly-created specialist's history from an
    /// exported package, not by ordinary task verification (which
    /// always wants to add one outcome to whatever's already there,
    /// not overwrite it).
    pub fn set_benchmark(&self, benchmark: &SpecialistBenchmark) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO specialist_benchmarks (specialist_id, verification_passed_count, verification_failed_count)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(specialist_id) DO UPDATE SET
                verification_passed_count = ?2,
                verification_failed_count = ?3",
            params![
                benchmark.specialist_id.to_string(),
                benchmark.verification_passed_count,
                benchmark.verification_failed_count,
            ],
        )?;
        Ok(())
    }

    /// Inserts a new local registry entry, deliberately no
    /// `project_id`: the registry is a personal library independent of
    /// any one project. See `solexa_domain::registry` for the full
    /// rationale.
    pub fn insert_registry_entry(&self, entry: &RegistryEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO registry_entries (id, kind, name, domain, capabilities_json, package_json, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.id.to_string(),
                serde_json::to_string(&entry.kind)?,
                entry.name,
                entry.domain,
                serde_json::to_string(&entry.capabilities)?,
                entry.package_json,
                entry.added_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_registry_entries(&self) -> Result<Vec<RegistryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM registry_entries ORDER BY added_at DESC")?;
        let rows = stmt
            .query_map([], registry_entry_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_registry_entry(&self, id: Uuid) -> Result<Option<RegistryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM registry_entries WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], registry_entry_from_row).optional()?)
    }

    /// A genuine, permanent delete, unlike every other removal
    /// function in this crate, there is no archive-style soft-delete
    /// for a registry entry, because there is no live routing or
    /// benchmark history tied to one that "never silently discard"
    /// would need to protect. See `solexa_domain::registry`'s doc
    /// comment.
    pub fn delete_registry_entry(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM registry_entries WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }

    /// Never fails on a specialist with no recorded attempts yet, an
    /// all-zero `SpecialistBenchmark` (which reports a neutral 0.5
    /// success rate) is the correct answer for "no history", not an
    /// error the caller has to special-case.
    pub fn get_benchmark(&self, specialist_id: Uuid) -> Result<SpecialistBenchmark> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM specialist_benchmarks WHERE specialist_id = ?1")?;
        Ok(stmt
            .query_row(params![specialist_id.to_string()], benchmark_from_row)
            .optional()?
            .unwrap_or_else(|| SpecialistBenchmark::empty(specialist_id)))
    }

    // ---- git sync (round 22, Phase 8) --------------------------------

    /// Upsert, one config per project (v1). Linking again (e.g. after
    /// choosing a different local clone) simply replaces the row.
    pub fn set_git_sync_config(&self, config: &GitSyncConfig) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO git_sync_configs (project_id, repo_path, linked_at, last_synced_commit, last_synced_snapshot_json, last_synced_at, require_review_for_incoming_changes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(project_id) DO UPDATE SET
                repo_path = ?2,
                linked_at = ?3,
                last_synced_commit = ?4,
                last_synced_snapshot_json = ?5,
                last_synced_at = ?6,
                require_review_for_incoming_changes = ?7",
            params![
                config.project_id.to_string(),
                config.repo_path,
                config.linked_at.to_rfc3339(),
                config.last_synced_commit,
                config.last_synced_snapshot_json,
                config.last_synced_at.map(|t| t.to_rfc3339()),
                config.require_review_for_incoming_changes,
            ],
        )?;
        Ok(())
    }

    pub fn get_git_sync_config(&self, project_id: Uuid) -> Result<Option<GitSyncConfig>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM git_sync_configs WHERE project_id = ?1")?;
        Ok(stmt.query_row(params![project_id.to_string()], git_sync_config_from_row).optional()?)
    }

    /// A genuine delete, like `delete_registry_entry`, this row is a
    /// pointer/config, not project substance the "never silently
    /// discard" rule protects. Unlinking doesn't touch any synced
    /// Member/Team/Specialist data, only the link itself.
    pub fn delete_git_sync_config(&self, project_id: Uuid) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM git_sync_configs WHERE project_id = ?1", params![project_id.to_string()])?;
        Ok(())
    }

    // ---- pending sync changes (round 46) -----------------------------

    /// Looks for an existing *Pending* row for this exact object, used
    /// by `Coordinator::pull_from_git` to refresh-in-place on a later
    /// pull instead of piling up duplicate pending rows for the same
    /// still-undecided object.
    pub fn get_pending_sync_change_for_object(&self, project_id: Uuid, kind: SyncObjectKind, object_id: Uuid) -> Result<Option<PendingSyncChange>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM pending_sync_changes WHERE project_id = ?1 AND kind = ?2 AND object_id = ?3 AND status = ?4",
        )?;
        Ok(stmt
            .query_row(
                params![
                    project_id.to_string(),
                    serde_json::to_string(&kind)?,
                    object_id.to_string(),
                    serde_json::to_string(&PendingSyncChangeStatus::Pending)?,
                ],
                pending_sync_change_from_row,
            )
            .optional()?)
    }

    pub fn insert_pending_sync_change(&self, change: &PendingSyncChange) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        insert_pending_sync_change_with_conn(&conn, change)
    }

    pub fn update_pending_sync_change(&self, change: &PendingSyncChange) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE pending_sync_changes SET label=?2, object_json=?3, benchmark_json=?4, is_new=?5,
                source_commit=?6, discovered_at=?7, status=?8, decided_by=?9, decided_at=?10, denial_reason=?11
             WHERE id = ?1",
            params![
                change.id.to_string(),
                change.label,
                serde_json::to_string(&change.object_json)?,
                change.benchmark_json.as_ref().map(serde_json::to_string).transpose()?,
                change.is_new,
                change.source_commit,
                change.discovered_at.to_rfc3339(),
                serde_json::to_string(&change.status)?,
                change.decided_by,
                change.decided_at.map(|t| t.to_rfc3339()),
                change.denial_reason,
            ],
        )?;
        Ok(())
    }

    pub fn get_pending_sync_change(&self, id: Uuid) -> Result<Option<PendingSyncChange>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM pending_sync_changes WHERE id = ?1")?;
        Ok(stmt.query_row(params![id.to_string()], pending_sync_change_from_row).optional()?)
    }

    /// Every pending-sync-change row for a project, oldest first,
    /// pending, applied, and denied alike, the same "keep the decided
    /// ones as history, don't just return the open queue" shape
    /// `list_conflicts` already uses.
    pub fn list_pending_sync_changes(&self, project_id: Uuid) -> Result<Vec<PendingSyncChange>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM pending_sync_changes WHERE project_id = ?1 ORDER BY discovered_at ASC")?;
        let rows = stmt
            .query_map(params![project_id.to_string()], pending_sync_change_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn git_sync_config_from_row(row: &Row) -> rusqlite::Result<GitSyncConfig> {
    let last_synced_commit: Option<String> = row.get("last_synced_commit")?;
    let last_synced_snapshot_json: Option<String> = row.get("last_synced_snapshot_json")?;
    let last_synced_at: Option<String> = row.get("last_synced_at")?;
    Ok(GitSyncConfig {
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?).expect("stored uuid is well-formed"),
        repo_path: row.get("repo_path")?,
        linked_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("linked_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        last_synced_commit,
        last_synced_snapshot_json,
        last_synced_at: last_synced_at.map(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .expect("stored timestamp is well-formed")
                .with_timezone(&chrono::Utc)
        }),
        require_review_for_incoming_changes: row.get("require_review_for_incoming_changes")?,
    })
}

fn insert_pending_sync_change_with_conn(conn: &Connection, change: &PendingSyncChange) -> Result<()> {
    conn.execute(
        "INSERT INTO pending_sync_changes (id, project_id, kind, object_id, label, object_json, benchmark_json,
            is_new, source_commit, discovered_at, status, decided_by, decided_at, denial_reason)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            change.id.to_string(),
            change.project_id.to_string(),
            serde_json::to_string(&change.kind)?,
            change.object_id.to_string(),
            change.label,
            serde_json::to_string(&change.object_json)?,
            change.benchmark_json.as_ref().map(serde_json::to_string).transpose()?,
            change.is_new,
            change.source_commit,
            change.discovered_at.to_rfc3339(),
            serde_json::to_string(&change.status)?,
            change.decided_by,
            change.decided_at.map(|t| t.to_rfc3339()),
            change.denial_reason,
        ],
    )?;
    Ok(())
}

fn pending_sync_change_from_row(row: &Row) -> rusqlite::Result<PendingSyncChange> {
    let object_json_raw: String = row.get("object_json")?;
    let benchmark_json_raw: Option<String> = row.get("benchmark_json")?;
    let discovered_at: String = row.get("discovered_at")?;
    let decided_at: Option<String> = row.get("decided_at")?;
    Ok(PendingSyncChange {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?).expect("stored uuid is well-formed"),
        kind: serde_json::from_str(&row.get::<_, String>("kind")?).expect("stored sync object kind is well-formed"),
        object_id: Uuid::parse_str(&row.get::<_, String>("object_id")?).expect("stored uuid is well-formed"),
        label: row.get("label")?,
        object_json: serde_json::from_str(&object_json_raw).expect("stored object_json is well-formed"),
        benchmark_json: benchmark_json_raw.map(|s| serde_json::from_str(&s).expect("stored benchmark_json is well-formed")),
        is_new: row.get("is_new")?,
        source_commit: row.get("source_commit")?,
        discovered_at: chrono::DateTime::parse_from_rfc3339(&discovered_at)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        status: serde_json::from_str(&row.get::<_, String>("status")?).expect("stored pending-sync-change status is well-formed"),
        decided_by: row.get("decided_by")?,
        decided_at: decided_at.map(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .expect("stored timestamp is well-formed")
                .with_timezone(&chrono::Utc)
        }),
        denial_reason: row.get("denial_reason")?,
    })
}

/// Wires the event bus straight into the `events` table: every published
/// event is durably recorded here before it's considered "published".
impl EventSink for Storage {
    fn record(&self, event: &Event) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (event_id, project_id, timestamp, actor_id, actor_type,
                event_type, payload_json, correlation_id, causation_id)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                event.event_id.to_string(),
                event.project_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.actor_id,
                serde_json::to_string(&event.actor_type)?,
                serde_json::to_string(&event.event_type)?,
                serde_json::to_string(&event.payload)?,
                event.correlation_id.to_string(),
                event.causation_id.map(|id| id.to_string()),
            ],
        )?;
        Ok(())
    }
}

// ---- row <-> domain-type mapping -----------------------------------------

/// Builds the positional parameter list shared by the INSERT and UPDATE
/// task statements (they address the same 17 columns in the same
/// order, `deadline` appended at the end in round 44 rather than
/// inserted in field-declaration order, so every existing placeholder
/// number above stays exactly what it already was). Returned as
/// `rusqlite::types::Value`, an owned, dynamically typed value that
/// itself implements `ToSql`, so it can be fed to `params_from_iter`
/// without the trait-object plumbing `Box<dyn ToSql>` would require.
fn task_params(task: &Task) -> Result<Vec<Value>> {
    let optional_json = |v: Option<String>| -> Value {
        v.map(Value::Text).unwrap_or(Value::Null)
    };
    Ok(vec![
        Value::Text(task.id.to_string()),
        Value::Text(task.project_id.to_string()),
        Value::Text(task.title.clone()),
        Value::Text(task.description.clone()),
        Value::Text(serde_json::to_string(&task.state)?),
        optional_json(task.owner_id.map(|id| id.to_string())),
        optional_json(task.assigned_specialist_id.map(|id| id.to_string())),
        optional_json(task.parent_task_id.map(|id| id.to_string())),
        Value::Text(serde_json::to_string(&task.required_capabilities)?),
        Value::Integer(task.priority as i64),
        optional_json(
            task.context_package
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ),
        optional_json(task.result.as_ref().map(serde_json::to_string).transpose()?),
        optional_json(
            task.confidence
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ),
        optional_json(
            task.verification
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ),
        Value::Text(task.created_at.to_rfc3339()),
        Value::Text(task.updated_at.to_rfc3339()),
        optional_json(task.deadline.map(|d| d.to_rfc3339())),
    ])
}

fn task_from_row(row: &Row) -> rusqlite::Result<Task> {
    let json = |col: &str| -> rusqlite::Result<Option<String>> { row.get(col) };
    let parse_uuid = |s: String| Uuid::parse_str(&s).expect("stored uuid is well-formed");

    Ok(Task {
        id: parse_uuid(row.get("id")?),
        project_id: parse_uuid(row.get("project_id")?),
        title: row.get("title")?,
        description: row.get("description")?,
        state: serde_json::from_str::<TaskState>(&row.get::<_, String>("state")?)
            .expect("stored task state is well-formed"),
        owner_id: json("owner_id")?.map(parse_uuid),
        assigned_specialist_id: json("assigned_specialist_id")?.map(parse_uuid),
        parent_task_id: json("parent_task_id")?.map(parse_uuid),
        required_capabilities: serde_json::from_str(
            &row.get::<_, String>("required_capabilities_json")?,
        )
        .expect("stored capabilities are well-formed"),
        priority: row.get("priority")?,
        context_package: json("context_package_json")?
            .map(|s| serde_json::from_str::<ContextPackage>(&s))
            .transpose()
            .expect("stored context package is well-formed"),
        result: json("result_json")?
            .map(|s| serde_json::from_str::<TaskResult>(&s))
            .transpose()
            .expect("stored result is well-formed"),
        confidence: json("confidence_json")?
            .map(|s| serde_json::from_str::<Confidence>(&s))
            .transpose()
            .expect("stored confidence is well-formed"),
        verification: json("verification_json")?
            .map(|s| serde_json::from_str::<VerificationOutcome>(&s))
            .transpose()
            .expect("stored verification is well-formed"),
        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("created_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        updated_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("updated_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        deadline: json("deadline")?
            .map(|s| chrono::DateTime::parse_from_rfc3339(&s))
            .transpose()
            .expect("stored deadline is well-formed")
            .map(|d| d.with_timezone(&chrono::Utc)),
    })
}

fn model_from_row(row: &Row) -> rusqlite::Result<ModelMetadata> {
    Ok(ModelMetadata {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        name: row.get("name")?,
        provider: row.get("provider")?,
        modalities: serde_json::from_str(&row.get::<_, String>("modalities_json")?)
            .expect("stored modalities are well-formed"),
        context_capacity_tokens: row.get("context_capacity_tokens")?,
        parameter_count_billions: row.get("parameter_count_billions")?,
        quantization: row.get("quantization")?,
        estimated_vram_mb: row.get("estimated_vram_mb")?,
        estimated_ram_mb: row.get("estimated_ram_mb")?,
        tool_support: row.get("tool_support")?,
        structured_output_support: row.get("structured_output_support")?,
        streaming_support: row.get("streaming_support")?,
        local: row.get("local")?,
        license: row.get("license")?,
    })
}

fn specialist_from_row(row: &Row) -> rusqlite::Result<Specialist> {
    let preferred_model_ids: Vec<String> =
        serde_json::from_str(&row.get::<_, String>("preferred_model_ids_json")?)
            .expect("stored model ids are well-formed");
    let output_schema_json: Option<String> = row.get("output_schema_json")?;

    Ok(Specialist {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        name: row.get("name")?,
        role: row.get("role")?,
        domain: row.get("domain")?,
        capabilities: serde_json::from_str(&row.get::<_, String>("capabilities_json")?)
            .expect("stored capabilities are well-formed"),
        preferred_model_ids: preferred_model_ids
            .into_iter()
            .map(|s| Uuid::parse_str(&s).expect("stored uuid is well-formed"))
            .collect(),
        tools: serde_json::from_str(&row.get::<_, String>("tools_json")?)
            .expect("stored tools are well-formed"),
        knowledge_scope: serde_json::from_str(&row.get::<_, String>("knowledge_scope_json")?)
            .expect("stored knowledge scope is well-formed"),
        permissions: serde_json::from_str::<Vec<Permission>>(
            &row.get::<_, String>("permissions_json")?,
        )
        .expect("stored permissions are well-formed"),
        output_schema: output_schema_json
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .expect("stored output schema is well-formed"),
        escalation_policy: serde_json::from_str::<EscalationPolicy>(
            &row.get::<_, String>("escalation_policy_json")?,
        )
        .expect("stored escalation policy is well-formed"),
        active: row.get("active")?,
        role_ids: serde_json::from_str(&row.get::<_, String>("role_ids_json")?)
            .expect("stored role ids are well-formed"),
        max_tokens: row.get("max_tokens")?,
        temperature: row.get::<_, Option<f64>>("temperature")?.map(|t| t as f32),
    })
}

fn event_from_row(row: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        event_id: Uuid::parse_str(&row.get::<_, String>("event_id")?)
            .expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?)
            .expect("stored uuid is well-formed"),
        timestamp: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("timestamp")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        actor_id: row.get("actor_id")?,
        actor_type: serde_json::from_str::<ActorType>(&row.get::<_, String>("actor_type")?)
            .expect("stored actor type is well-formed"),
        event_type: serde_json::from_str::<EventType>(&row.get::<_, String>("event_type")?)
            .expect("stored event type is well-formed"),
        payload: serde_json::from_str(&row.get::<_, String>("payload_json")?)
            .expect("stored payload is well-formed"),
        correlation_id: Uuid::parse_str(&row.get::<_, String>("correlation_id")?)
            .expect("stored uuid is well-formed"),
        causation_id: row
            .get::<_, Option<String>>("causation_id")?
            .map(|s| Uuid::parse_str(&s).expect("stored uuid is well-formed")),
    })
}

fn knowledge_item_from_row(row: &Row) -> rusqlite::Result<KnowledgeItem> {
    Ok(KnowledgeItem {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?)
            .expect("stored uuid is well-formed"),
        kind: serde_json::from_str::<KnowledgeKind>(&row.get::<_, String>("kind")?)
            .expect("stored knowledge kind is well-formed"),
        title: row.get("title")?,
        content: row.get("content")?,
        status: serde_json::from_str::<KnowledgeStatus>(&row.get::<_, String>("status")?)
            .expect("stored knowledge status is well-formed"),
        provenance: serde_json::from_str::<Provenance>(&row.get::<_, String>("provenance_json")?)
            .expect("stored provenance is well-formed"),
        tags: serde_json::from_str(&row.get::<_, String>("tags_json")?)
            .expect("stored tags are well-formed"),
        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("created_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
    })
}

fn decision_from_row(row: &Row) -> rusqlite::Result<Decision> {
    Ok(Decision {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?)
            .expect("stored uuid is well-formed"),
        question: row.get("question")?,
        options: serde_json::from_str(&row.get::<_, String>("options_json")?)
            .expect("stored options are well-formed"),
        resolution: row.get("resolution")?,
        approved_by: row.get("approved_by")?,
        status: serde_json::from_str::<DecisionStatus>(&row.get::<_, String>("status")?)
            .expect("stored decision status is well-formed"),
        tags: serde_json::from_str(&row.get::<_, String>("tags_json")?)
            .expect("stored tags are well-formed"),
        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("created_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        github_issue_url: row.get("github_issue_url")?,
    })
}

fn conflict_from_row(row: &Row) -> rusqlite::Result<Conflict> {
    let task_id_raw: String = row.get("task_id")?;
    let sync_kind_raw: Option<String> = row.get("sync_object_kind")?;
    let sync_id_raw: Option<String> = row.get("sync_object_id")?;
    let sync_field_raw: Option<String> = row.get("sync_object_field")?;
    Ok(Conflict {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?)
            .expect("stored uuid is well-formed"),
        task_id: if task_id_raw.is_empty() {
            None
        } else {
            Some(Uuid::parse_str(&task_id_raw).expect("stored uuid is well-formed"))
        },
        question: row.get("question")?,
        positions: serde_json::from_str(&row.get::<_, String>("positions_json")?)
            .expect("stored positions are well-formed"),
        resolution: row.get("resolution")?,
        approved_by: row.get("approved_by")?,
        status: serde_json::from_str::<ConflictStatus>(&row.get::<_, String>("status")?)
            .expect("stored conflict status is well-formed"),
        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("created_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
        sync_object: match (sync_kind_raw, sync_id_raw) {
            (Some(kind), Some(id)) => Some(SyncObjectRef {
                kind: serde_json::from_str::<SyncObjectKind>(&kind).expect("stored sync object kind is well-formed"),
                object_id: Uuid::parse_str(&id).expect("stored uuid is well-formed"),
                field: sync_field_raw,
            }),
            _ => None,
        },
        github_pr_url: row.get("github_pr_url")?,
    })
}

fn member_from_row(row: &Row) -> rusqlite::Result<Member> {
    let role_ids_raw: String = row.get("role_ids_json")?;
    Ok(Member {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?)
            .expect("stored uuid is well-formed"),
        display_name: row.get("display_name")?,
        description: row.get("description")?,
        member_type: serde_json::from_str::<MemberType>(&row.get::<_, String>("member_type")?)
            .expect("stored member type is well-formed"),
        specialist_ids: serde_json::from_str(&row.get::<_, String>("specialist_ids_json")?)
            .expect("stored specialist ids are well-formed"),
        deleted_at: parse_optional_timestamp(row.get("deleted_at")?),
        role_ids: serde_json::from_str(&role_ids_raw).expect("stored role ids are well-formed"),
    })
}

fn role_from_row(row: &Row) -> rusqlite::Result<Role> {
    Ok(Role {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?).expect("stored uuid is well-formed"),
        name: row.get("name")?,
        permissions: serde_json::from_str(&row.get::<_, String>("permissions_json")?).expect("stored permissions are well-formed"),
        created_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("created_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
    })
}

fn team_from_row(row: &Row) -> rusqlite::Result<Team> {
    Ok(Team {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        project_id: Uuid::parse_str(&row.get::<_, String>("project_id")?)
            .expect("stored uuid is well-formed"),
        name: row.get("name")?,
        mission: row.get("mission")?,
        member_ids: serde_json::from_str(&row.get::<_, String>("member_ids_json")?)
            .expect("stored member ids are well-formed"),
        parent_team_id: row
            .get::<_, Option<String>>("parent_team_id")?
            .map(|s| Uuid::parse_str(&s).expect("stored uuid is well-formed")),
        deleted_at: parse_optional_timestamp(row.get("deleted_at")?),
    })
}

/// Shared by every row-mapper with a nullable RFC3339 timestamp column
/// (round 27's `deleted_at` on `members`/`teams`).
fn parse_optional_timestamp(raw: Option<String>) -> Option<chrono::DateTime<chrono::Utc>> {
    raw.map(|s| {
        chrono::DateTime::parse_from_rfc3339(&s)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc)
    })
}

fn benchmark_from_row(row: &Row) -> rusqlite::Result<SpecialistBenchmark> {
    Ok(SpecialistBenchmark {
        specialist_id: Uuid::parse_str(&row.get::<_, String>("specialist_id")?)
            .expect("stored uuid is well-formed"),
        verification_passed_count: row.get("verification_passed_count")?,
        verification_failed_count: row.get("verification_failed_count")?,
    })
}

fn registry_entry_from_row(row: &Row) -> rusqlite::Result<RegistryEntry> {
    Ok(RegistryEntry {
        id: Uuid::parse_str(&row.get::<_, String>("id")?).expect("stored uuid is well-formed"),
        kind: serde_json::from_str::<RegistryEntryKind>(&row.get::<_, String>("kind")?)
            .expect("stored registry entry kind is well-formed"),
        name: row.get("name")?,
        domain: row.get("domain")?,
        capabilities: serde_json::from_str(&row.get::<_, String>("capabilities_json")?)
            .expect("stored capabilities are well-formed"),
        package_json: row.get("package_json")?,
        added_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>("added_at")?)
            .expect("stored timestamp is well-formed")
            .with_timezone(&chrono::Utc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use solexa_domain::{Decision, DecisionStatus, GitSyncConfig, KnowledgeItem, KnowledgeKind, KnowledgeStatus, Provenance, Task};

    #[test]
    fn get_project_returns_none_for_an_unknown_id_and_the_row_once_inserted() {
        let storage = Storage::in_memory().unwrap();
        assert!(storage.get_project(Uuid::new_v4()).unwrap().is_none());

        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "SOLEXA", "an AI team engine").unwrap();
        let found = storage.get_project(project_id).unwrap().unwrap();
        assert_eq!(found.name, "SOLEXA");
        assert_eq!(found.description, "an AI team engine");
    }

    #[test]
    fn list_projects_returns_every_project_and_keeps_them_isolated() {
        let storage = Storage::in_memory().unwrap();
        assert!(storage.list_projects().unwrap().is_empty());

        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        storage.insert_project(first, "Website Redesign", "").unwrap();
        storage.insert_project(second, "Compiler Work", "").unwrap();

        let projects = storage.list_projects().unwrap();
        assert_eq!(projects.len(), 2);
        assert!(projects.iter().any(|p| p.id == first && p.name == "Website Redesign"));
        assert!(projects.iter().any(|p| p.id == second && p.name == "Compiler Work"));

        // The isolation the whole multi-project story rests on: rows
        // written under one project never leak into another's queries.
        // This has always been true (every table carries project_id),
        // but nothing asserted it until there was a way to actually
        // have two projects in the app at once.
        storage.insert_member(&Member::new_human(first, "Alex")).unwrap();
        assert_eq!(storage.list_members(first).unwrap().len(), 1);
        assert!(storage.list_members(second).unwrap().is_empty());
    }

    fn sample_model() -> ModelMetadata {
        ModelMetadata {
            id: Uuid::new_v4(),
            name: "llama3:8b".into(),
            provider: "ollama".into(),
            modalities: vec![solexa_domain::Modality::Text],
            context_capacity_tokens: 8192,
            parameter_count_billions: Some(8.0),
            quantization: Some("Q4_0".into()),
            estimated_vram_mb: 4096,
            estimated_ram_mb: 8192,
            tool_support: false,
            structured_output_support: false,
            streaming_support: true,
            local: true,
            license: None,
        }
    }

    #[test]
    fn insert_model_round_trips_every_field_through_get_model() {
        let storage = Storage::in_memory().unwrap();
        let model = sample_model();
        storage.insert_model(&model).unwrap();

        let found = storage.get_model(model.id).unwrap().unwrap();
        assert_eq!(found.name, model.name);
        assert_eq!(found.provider, model.provider);
        assert_eq!(found.modalities, model.modalities);
        assert_eq!(found.context_capacity_tokens, model.context_capacity_tokens);
        assert_eq!(found.tool_support, model.tool_support);
        assert_eq!(found.streaming_support, model.streaming_support);
        assert_eq!(found.local, model.local);
        assert!(storage.get_model(Uuid::new_v4()).unwrap().is_none());
    }

    #[test]
    fn a_model_is_enabled_for_every_project_until_explicitly_disabled() {
        // Round 41: the default has to be "everything available",
        // this is the assertion that keeps that default honest.
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let model = sample_model();
        storage.insert_model(&model).unwrap();

        assert!(storage.disabled_model_ids_for_project(project_id).unwrap().is_empty());

        storage.set_model_enabled_for_project(project_id, model.id, false).unwrap();
        assert_eq!(storage.disabled_model_ids_for_project(project_id).unwrap(), vec![model.id]);

        storage.set_model_enabled_for_project(project_id, model.id, true).unwrap();
        assert!(storage.disabled_model_ids_for_project(project_id).unwrap().is_empty());
    }

    #[test]
    fn disabling_a_model_in_one_project_never_affects_another() {
        let storage = Storage::in_memory().unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        storage.insert_project(first, "Website Redesign", "").unwrap();
        storage.insert_project(second, "Compiler Work", "").unwrap();
        let model = sample_model();
        storage.insert_model(&model).unwrap();

        storage.set_model_enabled_for_project(first, model.id, false).unwrap();

        assert_eq!(storage.disabled_model_ids_for_project(first).unwrap(), vec![model.id]);
        assert!(storage.disabled_model_ids_for_project(second).unwrap().is_empty());
    }

    #[test]
    fn disabling_and_enabling_the_same_model_twice_is_not_an_error() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let model = sample_model();
        storage.insert_model(&model).unwrap();

        storage.set_model_enabled_for_project(project_id, model.id, false).unwrap();
        storage.set_model_enabled_for_project(project_id, model.id, false).unwrap();
        assert_eq!(storage.disabled_model_ids_for_project(project_id).unwrap().len(), 1);

        storage.set_model_enabled_for_project(project_id, model.id, true).unwrap();
        storage.set_model_enabled_for_project(project_id, model.id, true).unwrap();
        assert!(storage.disabled_model_ids_for_project(project_id).unwrap().is_empty());
    }

    #[test]
    fn round_trips_a_git_sync_config_and_upserts_on_relink() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        assert!(storage.get_git_sync_config(project_id).unwrap().is_none());

        let config = GitSyncConfig::new(project_id, "/home/alex/solexa-team-repo");
        storage.set_git_sync_config(&config).unwrap();
        let fetched = storage.get_git_sync_config(project_id).unwrap().unwrap();
        assert_eq!(fetched.repo_path, "/home/alex/solexa-team-repo");
        assert!(fetched.last_synced_commit.is_none());

        // Relinking (e.g. pointing at a different local clone) is an
        // upsert, not a second row.
        let mut relinked = GitSyncConfig::new(project_id, "/home/alex/another-clone");
        relinked.last_synced_commit = Some("abc123".into());
        storage.set_git_sync_config(&relinked).unwrap();
        let fetched = storage.get_git_sync_config(project_id).unwrap().unwrap();
        assert_eq!(fetched.repo_path, "/home/alex/another-clone");
        assert_eq!(fetched.last_synced_commit.as_deref(), Some("abc123"));

        storage.delete_git_sync_config(project_id).unwrap();
        assert!(storage.get_git_sync_config(project_id).unwrap().is_none());
    }

    #[test]
    fn round_trips_a_task_through_sqlite() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let mut task = Task::new(project_id, "Do the thing", "details");
        task.required_capabilities = vec!["general".into()];
        // Round 44: exercises deadline round-tripping through the same
        // insert/update/get path everything else in this test uses,
        // not a separate write path that could silently diverge.
        let deadline = chrono::Utc::now() + chrono::Duration::hours(6);
        task.deadline = Some(deadline);
        storage.insert_task(&task).unwrap();

        task.transition_to(TaskState::Planned).unwrap();
        storage.update_task(&task).unwrap();

        let fetched = storage.get_task(task.id).unwrap().unwrap();
        assert_eq!(fetched.state, TaskState::Planned);
        assert_eq!(fetched.title, "Do the thing");
        assert_eq!(fetched.required_capabilities, vec!["general".to_string()]);
        // RFC 3339 round-trips to the microsecond; comparing timestamps
        // is otherwise prone to false failures over sub-second jitter.
        assert_eq!(fetched.deadline.unwrap().timestamp_micros(), deadline.timestamp_micros());
    }

    #[test]
    fn a_task_with_no_deadline_round_trips_as_none_not_a_sentinel_value() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let task = Task::new(project_id, "Do the thing", "details");
        storage.insert_task(&task).unwrap();

        assert_eq!(storage.get_task(task.id).unwrap().unwrap().deadline, None);
    }

    #[test]
    fn migrates_an_older_tasks_table_missing_priority_and_deadline() {
        // Simulates opening a database created before round 36 added
        // `priority` directly to schema.sql with no migration entry (a
        // real gap this round also fixes, see `migrate`'s own doc
        // comment) and before round 44 added `deadline` the correct
        // way from the start. Both must come back via the ALTER TABLE
        // path without touching the existing row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
             CREATE TABLE tasks (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
                description TEXT NOT NULL, state TEXT NOT NULL, owner_id TEXT,
                assigned_specialist_id TEXT, parent_task_id TEXT,
                required_capabilities_json TEXT NOT NULL, context_package_json TEXT,
                result_json TEXT, confidence_json TEXT, verification_json TEXT,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
             );",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, project_id, title, description, state, required_capabilities_json, created_at, updated_at)
             VALUES (?1, ?2, 'pre-existing task', '', '\"created\"', '[]', ?3, ?3)",
            params![task_id.to_string(), project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        let fetched = storage.get_task(task_id).unwrap().unwrap();
        assert_eq!(fetched.title, "pre-existing task");
        assert_eq!(fetched.priority, 5, "the DEFAULT 5 on the ALTER TABLE must apply to the pre-existing row");
        assert_eq!(fetched.deadline, None);

        // And the migrated table must still be usable for a brand new
        // task going forward, not just readable.
        let mut new_task = Task::new(project_id, "New task", "");
        new_task.deadline = Some(chrono::Utc::now());
        storage.insert_task(&new_task).unwrap();
        assert!(storage.get_task(new_task.id).unwrap().unwrap().deadline.is_some());
    }

    #[test]
    fn events_are_queryable_by_correlation_id() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        let correlation_id = Uuid::new_v4();

        storage
            .record(&Event::new(
                project_id,
                "planner",
                ActorType::System,
                EventType::PlannerStarted,
                serde_json::json!({}),
                correlation_id,
                None,
            ))
            .unwrap();
        storage
            .record(&Event::new(
                project_id,
                "planner",
                ActorType::System,
                EventType::TaskCreated,
                serde_json::json!({}),
                correlation_id,
                None,
            ))
            .unwrap();

        let events = storage.list_events_for_correlation(correlation_id).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn round_trips_knowledge_items_and_decisions_through_sqlite() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let constraint = KnowledgeItem::new(
            project_id,
            KnowledgeKind::Constraint,
            "DB choice",
            "Must use PostgreSQL, not SQLite, in production.",
            Provenance::human("alice"),
            vec!["backend".into(), "sql".into()],
        );
        storage.insert_knowledge_item(&constraint).unwrap();

        let items = storage.list_knowledge_items(project_id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, KnowledgeKind::Constraint);
        assert_eq!(items[0].tags, vec!["backend".to_string(), "sql".to_string()]);
        assert_eq!(items[0].provenance.source, "human:alice");

        storage.update_knowledge_item_status(constraint.id, KnowledgeStatus::Archived).unwrap();
        let after = storage.list_knowledge_items(project_id).unwrap();
        assert_eq!(after.len(), 1, "archiving must not delete the row");
        assert_eq!(after[0].status, KnowledgeStatus::Archived);
        assert_eq!(after[0].content, constraint.content, "content is untouched by an archive");

        let mut decision = Decision::new(
            project_id,
            "Which database should be used?",
            vec!["PostgreSQL".into(), "SQLite".into()],
            vec!["backend".into()],
        );
        decision.resolve("PostgreSQL", "architecture-owner");
        storage.insert_decision(&decision).unwrap();

        let decisions = storage.list_decisions(project_id).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].status, DecisionStatus::Resolved);
        assert_eq!(decisions[0].resolution.as_deref(), Some("PostgreSQL"));
    }

    #[test]
    fn decision_can_be_resolved_after_it_was_already_stored() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let decision = Decision::new(
            project_id,
            "Which database should be used?",
            vec!["PostgreSQL".into(), "SQLite".into()],
            vec!["backend".into()],
        );
        storage.insert_decision(&decision).unwrap();
        assert_eq!(storage.get_decision(decision.id).unwrap().unwrap().status, DecisionStatus::Open);

        let mut fetched = storage.get_decision(decision.id).unwrap().unwrap();
        fetched.resolve("PostgreSQL", "architecture-owner");
        storage.update_decision(&fetched).unwrap();

        let resolved = storage.get_decision(decision.id).unwrap().unwrap();
        assert_eq!(resolved.status, DecisionStatus::Resolved);
        assert_eq!(resolved.approved_by.as_deref(), Some("architecture-owner"));
    }

    #[test]
    fn round_trips_a_conflict_through_sqlite() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let mut conflict = solexa_domain::Conflict::new(
            project_id,
            task_id,
            "Which result should be used?",
            vec![
                solexa_domain::ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Backend A".into(),
                    model_id: Some(Uuid::new_v4()),
                    content: serde_json::json!("answer A"),
                    verification_passed: Some(true),
                },
                solexa_domain::ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Backend B".into(),
                    model_id: Some(Uuid::new_v4()),
                    content: serde_json::json!("answer B"),
                    verification_passed: Some(true),
                },
            ],
        );
        storage.insert_conflict(&conflict).unwrap();

        let fetched = storage.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(fetched.positions.len(), 2);
        assert_eq!(fetched.status, solexa_domain::ConflictStatus::Open);
        assert_eq!(fetched.task_id, Some(task_id));
        assert!(fetched.sync_object.is_none());

        conflict.resolve("Accepted Backend A's result", "human-reviewer");
        storage.update_conflict(&conflict).unwrap();

        let resolved = storage.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(resolved.status, solexa_domain::ConflictStatus::Resolved);
        assert_eq!(resolved.approved_by.as_deref(), Some("human-reviewer"));
        // positions must survive resolution untouched, resolving never
        // discards the losing position.
        assert_eq!(resolved.positions.len(), 2);
    }

    #[test]
    fn round_trips_a_sync_conflict_with_no_task() {
        // A sync conflict (round 22) has no task_id and carries a
        // sync_object instead, the empty-string task_id sentinel and
        // the two new nullable columns must round-trip correctly.
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let conflict = solexa_domain::Conflict::new_sync(
            project_id,
            solexa_domain::SyncObjectRef { kind: solexa_domain::SyncObjectKind::Team, object_id: team_id, field: Some("mission".into()) },
            "Team \"Engineering\" was edited differently locally and remotely",
            vec![
                solexa_domain::ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Local".into(),
                    model_id: None,
                    content: serde_json::json!({"mission": "local mission"}),
                    verification_passed: None,
                },
                solexa_domain::ConflictPosition {
                    specialist_id: Uuid::new_v4(),
                    specialist_name: "Remote (origin/main)".into(),
                    model_id: None,
                    content: serde_json::json!({"mission": "remote mission"}),
                    verification_passed: None,
                },
            ],
        );
        storage.insert_conflict(&conflict).unwrap();

        let fetched = storage.get_conflict(conflict.id).unwrap().unwrap();
        assert_eq!(fetched.task_id, None);
        let sync_object = fetched.sync_object.expect("sync_object must round-trip");
        assert_eq!(sync_object.kind, solexa_domain::SyncObjectKind::Team);
        assert_eq!(sync_object.object_id, team_id);
        assert_eq!(sync_object.field.as_deref(), Some("mission"));
        assert_eq!(fetched.positions[0].model_id, None);
        assert_eq!(fetched.positions[0].verification_passed, None);
    }

    #[test]
    fn migrates_an_older_conflicts_table_missing_sync_columns() {
        // Simulates opening a database created by a pre-round-22 build:
        // a conflicts table without sync_object_kind/sync_object_id.
        // `Storage::open`'s migration step must add them without
        // touching any existing row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
             CREATE TABLE conflicts (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, task_id TEXT NOT NULL,
                question TEXT NOT NULL, positions_json TEXT NOT NULL, resolution TEXT,
                approved_by TEXT, status TEXT NOT NULL, created_at TEXT NOT NULL
             );",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let conflict_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conflicts (id, project_id, task_id, question, positions_json, status, created_at)
             VALUES (?1, ?2, ?3, 'pre-existing question', '[]', '\"open\"', ?4)",
            params![conflict_id.to_string(), project_id.to_string(), task_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();

        // This is exactly what `Storage::from_connection` does, schema
        // (idempotent, adds only new tables) then the migration step.
        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        let fetched = storage.get_conflict(conflict_id).unwrap().unwrap();
        assert_eq!(fetched.question, "pre-existing question");
        assert_eq!(fetched.task_id, Some(task_id));
        assert!(fetched.sync_object.is_none());
    }

    #[test]
    fn round_trips_members_and_nested_teams_through_sqlite() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let human = solexa_domain::Member::new_human(project_id, "Alex");
        let ai = solexa_domain::Member::new_ai(project_id, "Backend Bot", "runs the backend specialist");
        storage.insert_member(&human).unwrap();
        storage.insert_member(&ai).unwrap();

        let members = storage.list_members(project_id).unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(storage.get_member(human.id).unwrap().unwrap().display_name, "Alex");

        let parent_team = solexa_domain::Team::new(project_id, "Engineering", "ship the product")
            .with_members(vec![human.id]);
        storage.insert_team(&parent_team).unwrap();

        let child_team = solexa_domain::Team::new(project_id, "Backend", "own backend services")
            .nested_under(parent_team.id)
            .with_members(vec![ai.id]);
        storage.insert_team(&child_team).unwrap();

        let children = storage.list_child_teams(parent_team.id).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child_team.id);
        assert_eq!(storage.get_team(child_team.id).unwrap().unwrap().parent_team_id, Some(parent_team.id));
    }

    #[test]
    fn round_trips_a_soft_deleted_member_and_team() {
        // Round 27: deleted_at is a genuine, reversible soft delete,
        // never a hard delete of the row.
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let mut member = solexa_domain::Member::new_human(project_id, "Alex");
        storage.insert_member(&member).unwrap();
        assert!(!storage.get_member(member.id).unwrap().unwrap().is_deleted());

        let deleted_at = chrono::Utc::now();
        member.deleted_at = Some(deleted_at);
        storage.update_member(&member).unwrap();
        let fetched = storage.get_member(member.id).unwrap().unwrap();
        assert!(fetched.is_deleted());
        // RFC3339 round-trips to the same instant, even if sub-second
        // precision formatting differs.
        assert_eq!(fetched.deleted_at.unwrap().timestamp(), deleted_at.timestamp());

        // Restoring clears it back to None, the row, and everything
        // else about it, is exactly as it was.
        member.deleted_at = None;
        storage.update_member(&member).unwrap();
        assert!(!storage.get_member(member.id).unwrap().unwrap().is_deleted());
        assert_eq!(storage.get_member(member.id).unwrap().unwrap().display_name, "Alex");

        let mut team = solexa_domain::Team::new(project_id, "Engineering", "ship it");
        storage.insert_team(&team).unwrap();
        team.deleted_at = Some(chrono::Utc::now());
        storage.update_team(&team).unwrap();
        assert!(storage.get_team(team.id).unwrap().unwrap().is_deleted());
    }

    #[test]
    fn update_team_persists_a_membership_change_made_after_creation() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let mut team = solexa_domain::Team::new(project_id, "Sandbox Team", "handle whatever comes in");
        storage.insert_team(&team).unwrap();
        assert!(storage.get_team(team.id).unwrap().unwrap().member_ids.is_empty());

        let new_member_id = Uuid::new_v4();
        team.member_ids.push(new_member_id);
        storage.update_team(&team).unwrap();

        let reloaded = storage.get_team(team.id).unwrap().unwrap();
        assert_eq!(reloaded.member_ids, vec![new_member_id]);
    }

    #[test]
    fn benchmark_outcomes_accumulate_and_default_to_a_neutral_rate() {
        let storage = Storage::in_memory().unwrap();
        let specialist_id = Uuid::new_v4();

        // No history yet, neutral default, not zero and not an error.
        let empty = storage.get_benchmark(specialist_id).unwrap();
        assert_eq!(empty.attempts(), 0);
        assert_eq!(empty.success_rate(), 0.5);

        storage
            .record_benchmark_outcome(specialist_id, solexa_domain::BenchmarkOutcome::VerificationPassed)
            .unwrap();
        storage
            .record_benchmark_outcome(specialist_id, solexa_domain::BenchmarkOutcome::VerificationPassed)
            .unwrap();
        storage
            .record_benchmark_outcome(specialist_id, solexa_domain::BenchmarkOutcome::VerificationFailed)
            .unwrap();

        let after = storage.get_benchmark(specialist_id).unwrap();
        assert_eq!(after.verification_passed_count, 2);
        assert_eq!(after.verification_failed_count, 1);
        assert_eq!(after.attempts(), 3);
        assert!((after.success_rate() - (2.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn round_trips_a_role_and_assigns_it_to_a_member() {
        // Round 46.
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();

        let role = Role::new(project_id, "Manager", vec![Permission::Approve, Permission::Review]);
        storage.insert_role(&role).unwrap();
        let fetched = storage.get_role(role.id).unwrap().unwrap();
        assert_eq!(fetched.name, "Manager");
        assert_eq!(fetched.permissions, vec![Permission::Approve, Permission::Review]);

        let mut member = solexa_domain::Member::new_human(project_id, "Alex");
        assert!(member.role_ids.is_empty(), "no member is auto-granted a role just by existing");
        storage.insert_member(&member).unwrap();
        member.role_ids.push(role.id);
        storage.update_member(&member).unwrap();
        assert_eq!(storage.get_member(member.id).unwrap().unwrap().role_ids, vec![role.id]);

        // Renaming/re-permissioning a role updates every holder at
        // once, nothing about a member's own row needs to change.
        let mut renamed = fetched;
        renamed.name = "Lead".into();
        renamed.permissions = vec![Permission::Approve];
        storage.update_role(&renamed).unwrap();
        let after = storage.get_role(role.id).unwrap().unwrap();
        assert_eq!(after.name, "Lead");
        assert_eq!(after.permissions, vec![Permission::Approve]);
        assert_eq!(storage.get_member(member.id).unwrap().unwrap().role_ids, vec![role.id], "the member's own row is untouched by a role edit");

        assert_eq!(storage.list_roles(project_id).unwrap().len(), 1);
    }

    #[test]
    fn migrates_an_older_members_table_missing_role_ids() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
             CREATE TABLE members (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, display_name TEXT NOT NULL,
                description TEXT NOT NULL, member_type TEXT NOT NULL,
                specialist_ids_json TEXT NOT NULL, deleted_at TEXT
             );",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        let member_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO members (id, project_id, display_name, description, member_type, specialist_ids_json)
             VALUES (?1, ?2, 'pre-existing member', '', '\"human\"', '[]')",
            params![member_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        let fetched = storage.get_member(member_id).unwrap().unwrap();
        assert_eq!(fetched.display_name, "pre-existing member");
        assert!(fetched.role_ids.is_empty(), "the DEFAULT '[]' on the ALTER TABLE must apply to the pre-existing row");

        // And the migrated table is still usable for new rows, not
        // just readable.
        let mut role_holder = solexa_domain::Member::new_human(project_id, "New Member");
        role_holder.role_ids = vec![Uuid::new_v4()];
        storage.insert_member(&role_holder).unwrap();
        assert_eq!(storage.get_member(role_holder.id).unwrap().unwrap().role_ids.len(), 1);
    }

    #[test]
    fn migrates_an_older_specialists_table_missing_role_ids() {
        // Round 47, same shape as the members migration test above.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
             CREATE TABLE specialists (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, name TEXT NOT NULL, role TEXT NOT NULL,
                domain TEXT NOT NULL, capabilities_json TEXT NOT NULL, preferred_model_ids_json TEXT NOT NULL,
                tools_json TEXT NOT NULL, knowledge_scope_json TEXT NOT NULL, permissions_json TEXT NOT NULL,
                output_schema_json TEXT, escalation_policy_json TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1
             );",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        let specialist_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO specialists (id, project_id, name, role, domain, capabilities_json,
                preferred_model_ids_json, tools_json, knowledge_scope_json, permissions_json,
                escalation_policy_json)
             VALUES (?1, ?2, 'pre-existing specialist', 'General', '', '[]', '[]', '[]', '[]', '[\"read\"]',
                '{\"escalate_on_weak_confidence\":false,\"escalate_on_verification_failure\":false,\"requires_model_review\":false,\"requires_specialist_review\":false}')",
            params![specialist_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        let fetched = storage.get_specialist(specialist_id).unwrap().unwrap();
        assert_eq!(fetched.name, "pre-existing specialist");
        assert!(fetched.role_ids.is_empty(), "the DEFAULT '[]' on the ALTER TABLE must apply to the pre-existing row");

        // And an update through the migrated table can set role_ids,
        // not just read the migrated default.
        let mut updated = fetched;
        updated.role_ids = vec![Uuid::new_v4()];
        storage.update_specialist(&updated).unwrap();
        assert_eq!(storage.get_specialist(specialist_id).unwrap().unwrap().role_ids.len(), 1);
    }

    // ---- round 52: per-specialist generation limits -------------------

    /// Built from JSON on purpose: it is exactly what a pre-round-52
    /// serialized Specialist (no `max_tokens`/`temperature` keys) looks
    /// like, so the `#[serde(default)]` compatibility is exercised here too.
    fn pre_round_52_specialist_json(id: Uuid) -> Specialist {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "limits specialist", "role": "General", "domain": "",
            "capabilities": [], "preferred_model_ids": [], "tools": [], "knowledge_scope": [],
            "permissions": ["read"], "output_schema": null,
            "escalation_policy": {"escalate_on_weak_confidence": false, "escalate_on_verification_failure": false,
                                  "requires_model_review": false, "requires_specialist_review": false},
            "active": true, "role_ids": []
        }))
        .unwrap()
    }

    #[test]
    fn a_specialist_serialized_before_round_52_deserializes_with_no_limits() {
        let specialist = pre_round_52_specialist_json(Uuid::new_v4());
        assert_eq!(specialist.max_tokens, None);
        assert_eq!(specialist.temperature, None);
    }

    #[test]
    fn specialist_generation_limits_round_trip_through_insert_and_update() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();

        // Unset stays unset (NULL, not 0, 0 would be a real, useless limit).
        let mut specialist = pre_round_52_specialist_json(Uuid::new_v4());
        storage.insert_specialist(project_id, &specialist).unwrap();
        let fetched = storage.get_specialist(specialist.id).unwrap().unwrap();
        assert_eq!((fetched.max_tokens, fetched.temperature), (None, None));

        // Set via insert.
        let limited = Specialist { id: Uuid::new_v4(), max_tokens: Some(256), temperature: Some(0.7), ..specialist.clone() };
        storage.insert_specialist(project_id, &limited).unwrap();
        let fetched = storage.get_specialist(limited.id).unwrap().unwrap();
        assert_eq!(fetched.max_tokens, Some(256));
        assert_eq!(fetched.temperature, Some(0.7), "an f32 must survive the REAL column exactly");

        // Set via update, change via update, then clear via update.
        specialist.max_tokens = Some(64);
        specialist.temperature = Some(0.0);
        storage.update_specialist(&specialist).unwrap();
        let fetched = storage.get_specialist(specialist.id).unwrap().unwrap();
        assert_eq!((fetched.max_tokens, fetched.temperature), (Some(64), Some(0.0)), "temperature 0.0 is a real value, not 'unset'");
        specialist.max_tokens = None;
        specialist.temperature = None;
        storage.update_specialist(&specialist).unwrap();
        let fetched = storage.get_specialist(specialist.id).unwrap().unwrap();
        assert_eq!((fetched.max_tokens, fetched.temperature), (None, None), "clearing a limit must actually clear it");
    }

    #[test]
    fn migrates_an_older_specialists_table_missing_the_generation_limit_columns() {
        // A round-47..51 database: has role_ids_json, lacks max_tokens/temperature.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
             CREATE TABLE specialists (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL, name TEXT NOT NULL, role TEXT NOT NULL,
                domain TEXT NOT NULL, capabilities_json TEXT NOT NULL, preferred_model_ids_json TEXT NOT NULL,
                tools_json TEXT NOT NULL, knowledge_scope_json TEXT NOT NULL, permissions_json TEXT NOT NULL,
                output_schema_json TEXT, escalation_policy_json TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
                role_ids_json TEXT NOT NULL DEFAULT '[]'
             );",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        let specialist_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO specialists (id, project_id, name, role, domain, capabilities_json,
                preferred_model_ids_json, tools_json, knowledge_scope_json, permissions_json,
                escalation_policy_json)
             VALUES (?1, ?2, 'pre-existing specialist', 'General', '', '[]', '[]', '[]', '[]', '[\"read\"]',
                '{\"escalate_on_weak_confidence\":false,\"escalate_on_verification_failure\":false,\"requires_model_review\":false,\"requires_specialist_review\":false}')",
            params![specialist_id.to_string(), project_id.to_string()],
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        let fetched = storage.get_specialist(specialist_id).unwrap().unwrap();
        assert_eq!(fetched.name, "pre-existing specialist");
        assert_eq!((fetched.max_tokens, fetched.temperature), (None, None), "pre-existing rows read as unset, not as 0");

        // The migrated table can store limits, not just read the NULLs.
        let mut updated = fetched;
        updated.max_tokens = Some(128);
        updated.temperature = Some(0.25);
        storage.update_specialist(&updated).unwrap();
        let again = storage.get_specialist(specialist_id).unwrap().unwrap();
        assert_eq!((again.max_tokens, again.temperature), (Some(128), Some(0.25)));
    }

    // ---- round 55: opt-in approver-role enforcement ---------------------

    #[test]
    fn enforce_approver_roles_defaults_off_round_trips_and_errors_for_an_unknown_project() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        assert!(!storage.get_enforce_approver_roles(project_id).unwrap(), "a new project must NOT enforce, the opt-in is default-off");

        storage.set_enforce_approver_roles(project_id, true).unwrap();
        assert!(storage.get_enforce_approver_roles(project_id).unwrap());
        storage.set_enforce_approver_roles(project_id, false).unwrap();
        assert!(!storage.get_enforce_approver_roles(project_id).unwrap());

        // It is per project: another project is unaffected.
        let other = Uuid::new_v4();
        storage.insert_project(other, "Q", "").unwrap();
        storage.set_enforce_approver_roles(project_id, true).unwrap();
        assert!(!storage.get_enforce_approver_roles(other).unwrap());

        // Unknown project: an error, not a quiet `false` / a silent no-op.
        let ghost = Uuid::new_v4();
        assert!(storage.get_enforce_approver_roles(ghost).unwrap_err().to_string().contains("not found"));
        assert!(storage.set_enforce_approver_roles(ghost, true).unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn migrates_an_older_projects_table_missing_the_enforcement_column_as_not_enforced() {
        // A pre-round-55 database: `projects` has no enforce_approver_roles.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        assert!(
            !storage.get_enforce_approver_roles(project_id).unwrap(),
            "the DEFAULT 0 on the ALTER TABLE must apply to the pre-existing row, an existing project must not suddenly start enforcing"
        );
        // And the migrated column is writable, not just readable.
        storage.set_enforce_approver_roles(project_id, true).unwrap();
        assert!(storage.get_enforce_approver_roles(project_id).unwrap());
    }

    // ---- round 57: single-column task updates -------------------------

    #[test]
    fn a_stale_whole_row_task_write_regresses_state_but_the_single_column_setters_do_not() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let mut task = Task::new(project_id, "Do the thing", "details");
        storage.insert_task(&task).unwrap();

        // A caller reads the task while it is Created...
        let stale = storage.get_task(task.id).unwrap().unwrap();
        // ...and meanwhile the pipeline advances it.
        for next in [TaskState::Planned, TaskState::Queued, TaskState::Assigned, TaskState::Running] {
            task.transition_to(next).unwrap();
        }
        storage.update_task(&task).unwrap();

        // POSITIVE CONTROL, the hazard the setters exist to avoid: writing the
        // stale copy back (what read-modify-write does) silently undoes it.
        let mut stale_write = stale.clone();
        stale_write.priority = 9;
        storage.update_task(&stale_write).unwrap();
        assert_eq!(storage.get_task(task.id).unwrap().unwrap().state, TaskState::Created, "a whole-row write of a stale copy regresses the state, the lost update");

        // Restore the advanced state, then use the targeted setters.
        storage.update_task(&task).unwrap();
        assert!(storage.set_task_priority(task.id, 9).unwrap());
        let deadline = chrono::Utc::now() + chrono::Duration::hours(2);
        assert!(storage.set_task_deadline(task.id, Some(deadline)).unwrap());
        let after = storage.get_task(task.id).unwrap().unwrap();
        assert_eq!(after.priority, 9);
        assert_eq!(after.deadline.map(|d| d.timestamp()), Some(deadline.timestamp()));
        assert_eq!(after.state, TaskState::Running, "changing priority/deadline must not touch state");
        assert_eq!(after.updated_at.timestamp_millis(), task.updated_at.timestamp_millis());

        // Clearing a deadline really clears it; an unknown id reports false.
        assert!(storage.set_task_deadline(task.id, None).unwrap());
        assert!(storage.get_task(task.id).unwrap().unwrap().deadline.is_none());
        assert!(!storage.set_task_priority(Uuid::new_v4(), 3).unwrap());
        assert!(!storage.set_task_deadline(Uuid::new_v4(), None).unwrap());
    }

    #[test]
    fn migrates_an_older_git_sync_configs_table_missing_review_flag() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL);
             CREATE TABLE git_sync_configs (
                project_id TEXT PRIMARY KEY, repo_path TEXT NOT NULL, linked_at TEXT NOT NULL,
                last_synced_commit TEXT, last_synced_snapshot_json TEXT, last_synced_at TEXT
             );",
        )
        .unwrap();
        let project_id = Uuid::new_v4();
        conn.execute(
            "INSERT INTO projects (id, name, description, created_at) VALUES (?1, 'Old Project', '', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO git_sync_configs (project_id, repo_path, linked_at) VALUES (?1, '/repo', ?2)",
            params![project_id.to_string(), chrono::Utc::now().to_rfc3339()],
        )
        .unwrap();

        conn.execute_batch(SCHEMA).unwrap();
        Storage::migrate(&conn).unwrap();
        let storage = Storage { conn: Mutex::new(conn) };

        let fetched = storage.get_git_sync_config(project_id).unwrap().unwrap();
        assert_eq!(fetched.repo_path, "/repo");
        assert!(!fetched.require_review_for_incoming_changes, "the DEFAULT 0 on the ALTER TABLE must apply to the pre-existing row");
    }

    #[test]
    fn pending_sync_change_round_trips_and_dedups_by_object() {
        // Round 46.
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "Test Project", "").unwrap();
        let object_id = Uuid::new_v4();

        assert!(storage.get_pending_sync_change_for_object(project_id, SyncObjectKind::Member, object_id).unwrap().is_none());

        let change = PendingSyncChange::new(
            project_id,
            SyncObjectKind::Member,
            object_id,
            "Jordan",
            serde_json::json!({"display_name": "Jordan"}),
            None,
            true,
            "abc123",
        );
        storage.insert_pending_sync_change(&change).unwrap();

        let found = storage.get_pending_sync_change_for_object(project_id, SyncObjectKind::Member, object_id).unwrap().unwrap();
        assert_eq!(found.id, change.id);
        assert_eq!(found.label, "Jordan");
        assert_eq!(found.status, PendingSyncChangeStatus::Pending);
        assert!(found.is_new);

        // A later pull refreshes the SAME row in place rather than
        // creating a duplicate, this is exactly what
        // `Coordinator::pull_from_git` relies on to avoid piling up
        // repeat pending rows for a still-undecided object.
        let mut refreshed = found;
        refreshed.refresh_content("Jordan Lee", serde_json::json!({"display_name": "Jordan Lee"}), None, true, "def456");
        storage.update_pending_sync_change(&refreshed).unwrap();
        assert_eq!(storage.list_pending_sync_changes(project_id).unwrap().len(), 1, "refresh must not create a second row");
        let after_refresh = storage.get_pending_sync_change(change.id).unwrap().unwrap();
        assert_eq!(after_refresh.label, "Jordan Lee");
        assert_eq!(after_refresh.source_commit, "def456");

        // Deciding it (apply or deny) takes it out of the dedup lookup
        // a genuinely new incoming edit to the same object after
        // that should be free to queue as a fresh row.
        let mut decided = after_refresh;
        decided.apply("Alex (Manager)");
        storage.update_pending_sync_change(&decided).unwrap();
        assert!(storage.get_pending_sync_change_for_object(project_id, SyncObjectKind::Member, object_id).unwrap().is_none());
        assert_eq!(storage.get_pending_sync_change(change.id).unwrap().unwrap().status, PendingSyncChangeStatus::Applied);
    }

    // ---- round 61: app-level settings ------------------------------------

    #[test]
    fn an_unset_setting_reads_as_none() {
        let storage = Storage::in_memory().unwrap();
        assert_eq!(storage.get_setting("resource_budget").unwrap(), None);
    }

    #[test]
    fn a_setting_round_trips_and_a_second_write_replaces_it() {
        let storage = Storage::in_memory().unwrap();
        storage.set_setting("k", "one").unwrap();
        assert_eq!(storage.get_setting("k").unwrap().as_deref(), Some("one"));
        storage.set_setting("k", "two").unwrap();
        assert_eq!(storage.get_setting("k").unwrap().as_deref(), Some("two"), "an overwrite must replace, not duplicate or be ignored");
        storage.set_setting("other", "x").unwrap();
        assert_eq!(storage.get_setting("k").unwrap().as_deref(), Some("two"), "settings are independent keys");
    }

    #[test]
    fn a_setting_survives_closing_and_reopening_the_database_file() {
        let path = std::env::temp_dir().join(format!("solexa-settings-test-{}.sqlite3", uuid::Uuid::new_v4()));
        {
            let storage = Storage::open(&path).unwrap();
            storage.set_setting("resource_budget", "{\"vram_mb\":1}").unwrap();
        }
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(reopened.get_setting("resource_budget").unwrap().as_deref(), Some("{\"vram_mb\":1}"));
        drop(reopened);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleting_a_setting_removes_only_that_setting_and_is_a_noop_when_absent() {
        let storage = Storage::in_memory().unwrap();
        storage.set_setting("a", "1").unwrap();
        storage.set_setting("b", "2").unwrap();
        storage.delete_setting("a").unwrap();
        assert_eq!(storage.get_setting("a").unwrap(), None);
        assert_eq!(storage.get_setting("b").unwrap().as_deref(), Some("2"), "other settings are untouched");
        storage.delete_setting("a").unwrap();
        storage.delete_setting("never-set").unwrap();
    }

    // ---- round 64: heal task owners that hold a specialist id --------------

    fn task_owned_by(project_id: Uuid, title: &str, owner: Option<Uuid>) -> Task {
        let mut task = Task::new(project_id, title, "d");
        task.owner_id = owner;
        task
    }

    #[test]
    fn migration_clears_task_owners_that_are_specialist_ids_and_keeps_real_ones() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let specialist = pre_round_52_specialist_json(Uuid::new_v4());
        storage.insert_specialist(project_id, &specialist).unwrap();
        let member = solexa_domain::Member::new_ai(project_id, "Ava", "");
        storage.insert_member(&member).unwrap();

        // what rounds 1-63 wrote: the SPECIALIST's id in the member-id slot
        let wrong = task_owned_by(project_id, "wrong", Some(specialist.id));
        let right = task_owned_by(project_id, "right", Some(member.id));
        let unowned = task_owned_by(project_id, "none", None);
        for t in [&wrong, &right, &unowned] {
            storage.insert_task(t).unwrap();
        }

        Storage::migrate(&storage.conn.lock().unwrap()).unwrap();

        assert_eq!(storage.get_task(wrong.id).unwrap().unwrap().owner_id, None, "a specialist id is never a valid owner");
        assert_eq!(storage.get_task(right.id).unwrap().unwrap().owner_id, Some(member.id), "a real member owner must be left alone");
        assert_eq!(storage.get_task(unowned.id).unwrap().unwrap().owner_id, None);
    }

    #[test]
    fn the_owner_heal_is_idempotent_and_touches_only_owner_id() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let specialist = pre_round_52_specialist_json(Uuid::new_v4());
        storage.insert_specialist(project_id, &specialist).unwrap();
        let mut wrong = task_owned_by(project_id, "wrong", Some(specialist.id));
        wrong.assigned_specialist_id = Some(specialist.id);
        storage.insert_task(&wrong).unwrap();

        for _ in 0..3 {
            Storage::migrate(&storage.conn.lock().unwrap()).unwrap();
        }
        let after = storage.get_task(wrong.id).unwrap().unwrap();
        assert_eq!(after.owner_id, None);
        assert_eq!(after.assigned_specialist_id, Some(specialist.id), "the assignment itself is correct and must survive");
        assert_eq!(after.title, "wrong");
    }

    // ---- round 65: list queries have an explicit, documented order ---------
    //
    // Every list query below returns rows in INSERTION order (SQLite rowid).
    // That is what a plain scan happens to do today, so a test that merely
    // checks insertion order would pass even with no ORDER BY at all. Each
    // test therefore first adds an index that makes the *implicit* plan return
    // a different order (by id, which is made to run opposite to insertion
    // order), exactly the kind of future schema change the explicit ORDER BY
    // protects against, and only then checks the result.

    /// Makes `probe_<name>` the only index that can serve the lookup, so the
    /// implicit plan is forced onto it: any existing index it could tie with
    /// is dropped first (the planner's tie-break between equally good indexes
    /// is not something a test should depend on).
    fn add_order_probe_index(storage: &Storage, name: &str, table: &str, columns: &str, replacing: Option<&str>) {
        let conn = storage.conn.lock().unwrap();
        if let Some(existing) = replacing {
            conn.execute(&format!("DROP INDEX {existing}"), []).unwrap();
        }
        conn.execute(&format!("CREATE INDEX probe_{name} ON {table}({columns})"), []).unwrap();
    }

    /// Ids that sort in the OPPOSITE order to the order they are used in.
    fn descending_ids(n: u128) -> Vec<Uuid> {
        (0..n).map(|i| Uuid::from_u128(1_000 - i)).collect()
    }

    #[test]
    fn specialists_are_listed_in_creation_order_whatever_indexes_exist() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        add_order_probe_index(&storage, "specialists", "specialists", "project_id, id", None);
        let ids = descending_ids(6);
        for id in &ids {
            storage.insert_specialist(project_id, &pre_round_52_specialist_json(*id)).unwrap();
        }
        let listed: Vec<Uuid> = storage.list_specialists(project_id).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(listed, ids, "primary selection breaks ties by this order, so it must be creation order");
    }

    #[test]
    fn updating_a_specialist_does_not_move_it_in_the_list() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        let ids = descending_ids(4);
        for id in &ids {
            storage.insert_specialist(project_id, &pre_round_52_specialist_json(*id)).unwrap();
        }
        let mut first = storage.get_specialist(ids[0]).unwrap().unwrap();
        first.name = "renamed".into();
        first.active = false;
        storage.update_specialist(&first).unwrap();
        let listed: Vec<Uuid> = storage.list_specialists(project_id).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(listed, ids, "an edit must never change who counts as the 'first' specialist");
    }

    #[test]
    fn members_and_teams_are_listed_in_creation_order_whatever_indexes_exist() {
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        add_order_probe_index(&storage, "members", "members", "project_id, id", Some("idx_members_project"));
        add_order_probe_index(&storage, "teams", "teams", "project_id, id", Some("idx_teams_project"));
        add_order_probe_index(&storage, "teams_parent", "teams", "parent_team_id, id", Some("idx_teams_parent"));
        let ids = descending_ids(5);
        let parent = {
            let mut t = solexa_domain::Team::new(project_id, "parent", "");
            t.id = Uuid::from_u128(5_000);
            storage.insert_team(&t).unwrap();
            t.id
        };
        for (i, id) in ids.iter().enumerate() {
            let mut m = solexa_domain::Member::new_ai(project_id, format!("m{i}"), "");
            m.id = *id;
            storage.insert_member(&m).unwrap();
            let mut t = solexa_domain::Team::new(project_id, format!("t{i}"), "");
            t.id = *id;
            t.parent_team_id = Some(parent);
            storage.insert_team(&t).unwrap();
        }
        let members: Vec<Uuid> = storage.list_members(project_id).unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(members, ids);
        let teams: Vec<Uuid> = storage.list_teams(project_id).unwrap().into_iter().map(|t| t.id).filter(|id| *id != parent).collect();
        assert_eq!(teams, ids);
        let children: Vec<Uuid> = storage.list_child_teams(parent).unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(children, ids, "child teams too");
    }

    #[test]
    fn knowledge_decisions_and_conflicts_are_listed_in_creation_order_whatever_indexes_exist() {
        // Knowledge order decides prompt order AND which items survive the
        // context budget, so it must not depend on an index choice either.
        let storage = Storage::in_memory().unwrap();
        let project_id = Uuid::new_v4();
        storage.insert_project(project_id, "P", "").unwrap();
        add_order_probe_index(&storage, "knowledge_items", "knowledge_items", "project_id, id", Some("idx_knowledge_items_project"));
        add_order_probe_index(&storage, "decisions", "decisions", "project_id, id", Some("idx_decisions_project"));
        add_order_probe_index(&storage, "conflicts", "conflicts", "project_id, id", Some("idx_conflicts_project"));
        let ids = descending_ids(5);
        for (i, id) in ids.iter().enumerate() {
            let mut k = solexa_domain::KnowledgeItem::new(project_id, solexa_domain::KnowledgeKind::Fact, format!("t{i}"), format!("c{i}"), solexa_domain::Provenance::human("t"), vec![]);
            k.id = *id;
            storage.insert_knowledge_item(&k).unwrap();
            let mut d = solexa_domain::Decision::new(project_id, format!("q{i}"), vec!["a".into(), "b".into()], vec![]);
            d.id = *id;
            storage.insert_decision(&d).unwrap();
            let mut c = solexa_domain::Conflict::new(project_id, Uuid::new_v4(), format!("q{i}"), vec![]);
            c.id = *id;
            storage.insert_conflict(&c).unwrap();
        }
        let k: Vec<Uuid> = storage.list_knowledge_items(project_id).unwrap().into_iter().map(|x| x.id).collect();
        let d: Vec<Uuid> = storage.list_decisions(project_id).unwrap().into_iter().map(|x| x.id).collect();
        let c: Vec<Uuid> = storage.list_conflicts(project_id).unwrap().into_iter().map(|x| x.id).collect();
        assert_eq!((k, d, c), (ids.clone(), ids.clone(), ids));
    }

    #[test]
    fn models_are_listed_in_registration_order() {
        let storage = Storage::in_memory().unwrap();
        let mut ids = Vec::new();
        for i in 0..5u128 {
            let mut m = sample_model();
            m.id = Uuid::from_u128(1_000 - i);
            m.name = format!("model-{i}");
            ids.push(m.id);
            storage.insert_model(&m).unwrap();
        }
        let listed: Vec<Uuid> = storage.list_models().unwrap().into_iter().map(|m| m.id).collect();
        assert_eq!(listed, ids);
    }
}
