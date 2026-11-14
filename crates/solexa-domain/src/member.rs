//! Member, the primary unit of the system (spec section 2: "MODEL !=
//! MEMBER"). A member is an accountable participant: a human, an AI, a
//! human+AI hybrid, or a nested team. A `Specialist` (see specialist.rs)
//! is the capability configuration an AI `Member` runs.

use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberType {
    Human,
    Ai,
    Hybrid,
    Team,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: Uuid,
    pub project_id: Uuid,
    pub display_name: String,
    pub description: String,
    pub member_type: MemberType,
    /// For AI/Hybrid members: the Specialist configuration(s) this
    /// member runs. A human member's capabilities are asserted by a
    /// human/project owner rather than derived from a model.
    pub specialist_ids: Vec<Uuid>,
    /// Round 27: when this member was soft-deleted, `None` otherwise,
    /// see `Team::deleted_at`'s doc comment for the full rationale
    /// (never a hard delete, and a plain field specifically so sync
    /// propagates it through the existing per-field merge).
    pub deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Round 46: zero or more `Role`s this member holds, "Discord
    /// roles" for SOLEXA, a reusable named permission bundle rather
    /// than an ad-hoc list on each member. A member's effective
    /// permission set is the union across every role here (see
    /// `Coordinator::member_has_permission`). Empty by default,
    /// nobody is auto-granted anything just by existing.
    pub role_ids: Vec<Uuid>,
}

impl Member {
    pub fn new_ai(project_id: Uuid, display_name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            display_name: display_name.into(),
            description: description.into(),
            member_type: MemberType::Ai,
            specialist_ids: Vec::new(),
            deleted_at: None,
            role_ids: Vec::new(),
        }
    }

    pub fn new_human(project_id: Uuid, display_name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            display_name: display_name.into(),
            description: String::new(),
            member_type: MemberType::Human,
            specialist_ids: Vec::new(),
            deleted_at: None,
            role_ids: Vec::new(),
        }
    }

    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}
