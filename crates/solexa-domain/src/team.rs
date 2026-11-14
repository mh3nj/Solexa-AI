//! Team, a container of members, possibly nested (Phase 6 of the
//! roadmap).
//!
//! Spec: "Teams are containers of members. A team may contain: humans,
//! AI members, other teams... Support hierarchy. A team can delegate to
//! another team." Nesting is expressed via `parent_team_id` rather than
//! a team literally containing other `Team` values inline, that keeps
//! a team's own row simple and lets `solexa-coordinator` resolve the
//! hierarchy by query (see `Coordinator::team_members_recursive`)
//! instead of needing the whole tree loaded to construct any one team.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub mission: String,
    pub member_ids: Vec<Uuid>,
    /// `None` for a top-level team; `Some(parent)` for a nested team
    /// delegated to by `parent`.
    pub parent_team_id: Option<Uuid>,
    /// Round 27: when this team was soft-deleted, `None` otherwise.
    /// Never a hard delete, the row (and everything referencing it)
    /// stays exactly as it was, just hidden from normal views and
    /// excluded from routing, until `Coordinator::restore_team` clears
    /// this back to `None`. Kept as a plain field (not a separate
    /// status enum, unlike `Specialist::active`) specifically so git
    /// sync propagates a delete/restore through the exact same
    /// per-field 3-way merge every other field already gets, no new
    /// sync logic needed for this to work correctly across devices.
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Team {
    pub fn new(project_id: Uuid, name: impl Into<String>, mission: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            name: name.into(),
            mission: mission.into(),
            member_ids: Vec::new(),
            parent_team_id: None,
            deleted_at: None,
        }
    }

    pub fn nested_under(mut self, parent_team_id: Uuid) -> Self {
        self.parent_team_id = Some(parent_team_id);
        self
    }

    pub fn with_members(mut self, member_ids: Vec<Uuid>) -> Self {
        self.member_ids = member_ids;
        self
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}
