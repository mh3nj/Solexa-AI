//! Conflict, disagreement as a first-class, preserved object (Phase 5
//! of the roadmap, generalized in round 22 to cover Phase 8 sync
//! conflicts too).
//!
//! Spec rule (section 9, "Disagreement Is A Feature"): "Do not hide
//! disagreement. When specialists disagree, preserve the disagreement...
//! Never silently overwrite one position with another." When the
//! coordinator consults more than one specialist for the same task and
//! they produce genuinely different results, neither is thrown away,
//! both become `ConflictPosition`s on a `Conflict`, and the task waits
//! in `Review` until a human resolves it.
//!
//! Round 22 (git sync) reuses this exact type for a second situation:
//! two people editing the same Member/Team/Specialist while both
//! offline, then both syncing. The shape is generalized rather than
//! duplicated, `task_id` becomes optional (a sync conflict has no
//! task), and a `ConflictPosition` no longer requires a real specialist
//! or model (`model_id`/`verification_passed` become optional; a sync
//! position's `specialist_id` is a synthetic key that exists only to
//! let `resolve_sync_conflict` identify which side was chosen, and
//! `specialist_name` holds a human-readable label like "Local" or a
//! remote ref instead of an actual specialist's name). This is the
//! same "preserve both, never silently overwrite, let a human decide"
//! principle applied to a second kind of disagreement, not a
//! coincidentally similar new type.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictPosition {
    /// For a task-execution conflict, the specialist that produced this
    /// result. For a sync conflict, a synthetic id used only to select
    /// this position when resolving, it does not refer to any real
    /// specialist row.
    pub specialist_id: Uuid,
    /// A specialist's name for a task conflict; a human-readable label
    /// ("Local", or a remote ref such as "origin/main @ a1b2c3d") for a
    /// sync conflict.
    pub specialist_name: String,
    /// The model that produced this result, for a task-execution
    /// conflict. `None` for a sync conflict, no model is involved in
    /// two synced copies disagreeing.
    pub model_id: Option<Uuid>,
    pub content: serde_json::Value,
    /// Whether this result passed verification, for a task-execution
    /// conflict. `None` for a sync conflict, there is nothing to
    /// verify about a plain divergent value, only something to choose.
    pub verification_passed: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStatus {
    Open,
    Resolved,
}

/// Which kind of project object a sync conflict is actually about, and
/// its id, set only on conflicts created by `Coordinator::pull_from_git`
/// (see `Conflict::new_sync`). Round 22 covered the team roster
/// (Member/Team/Specialist); round 23 extended it to the Project
/// Brain's KnowledgeItem/Decision, see the round's README for what's
/// still out of scope (Tasks/Events, the local registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncObjectKind {
    Member,
    Team,
    Specialist,
    KnowledgeItem,
    Decision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncObjectRef {
    pub kind: SyncObjectKind,
    pub object_id: Uuid,
    /// Round 25: which specific field of the object is actually in
    /// dispute, a conflict now scopes to the one field that genuinely
    /// collided, not the whole object, so a Team edited on two
    /// non-overlapping fields by two devices merges automatically
    /// instead of producing one all-or-nothing conflict.
    ///
    /// `None` only ever occurs on a conflict created before round 25
    /// (back when a conflict always covered the whole object), every
    /// conflict this round creates always sets `Some(field_name)`.
    /// `resolve_sync_conflict` branches on this to stay compatible with
    /// any such conflict still sitting unresolved: `None` still applies
    /// the whole accepted position, `Some(field)` patches just that
    /// field into the object's *current* (already-merged) state.
    pub field: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub id: Uuid,
    pub project_id: Uuid,
    /// The task this conflict arose from verifying (Phase 5). `None`
    /// for a sync conflict (round 22), see `sync_object` instead.
    pub task_id: Option<Uuid>,
    pub question: String,
    pub positions: Vec<ConflictPosition>,
    pub resolution: Option<String>,
    pub approved_by: Option<String>,
    pub status: ConflictStatus,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Which project object this conflict is about, when it came from
    /// sync rather than task execution. `None` for a task-execution
    /// conflict, where `task_id` already identifies the disputed thing.
    pub sync_object: Option<SyncObjectRef>,
    /// Round 30: the GitHub PR opened for this conflict, if
    /// `Coordinator::send_conflict_to_github` has been called for it,
    /// `None` for every conflict never sent to GitHub (which remains
    /// the default, unremarkable path; nothing about round 30 makes
    /// sending a conflict to GitHub automatic or required).
    pub github_pr_url: Option<String>,
}

impl Conflict {
    /// A task-execution conflict (Phase 5): specialists disagreeing on
    /// a task's result.
    pub fn new(project_id: Uuid, task_id: Uuid, question: impl Into<String>, positions: Vec<ConflictPosition>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            task_id: Some(task_id),
            question: question.into(),
            positions,
            resolution: None,
            approved_by: None,
            status: ConflictStatus::Open,
            created_at: chrono::Utc::now(),
            sync_object: None,
            github_pr_url: None,
        }
    }

    /// A sync conflict (round 22, Phase 8): a Member/Team/Specialist
    /// edited differently on two devices between syncs. Resolved via
    /// `Coordinator::resolve_sync_conflict`, never `resolve_conflict`
    /// (which requires `task_id`).
    pub fn new_sync(project_id: Uuid, sync_object: SyncObjectRef, question: impl Into<String>, positions: Vec<ConflictPosition>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            task_id: None,
            question: question.into(),
            positions,
            resolution: None,
            approved_by: None,
            status: ConflictStatus::Open,
            created_at: chrono::Utc::now(),
            sync_object: Some(sync_object),
            github_pr_url: None,
        }
    }

    pub fn resolve(&mut self, resolution: impl Into<String>, approved_by: impl Into<String>) {
        self.resolution = Some(resolution.into());
        self.approved_by = Some(approved_by.into());
        self.status = ConflictStatus::Resolved;
    }

    pub fn position_for(&self, specialist_id: Uuid) -> Option<&ConflictPosition> {
        self.positions.iter().find(|p| p.specialist_id == specialist_id)
    }
}
