//! Role, a named, reusable permission bundle a Member can hold (round
//! 46), the "Discord roles" shape the user asked for explicitly:
//! several members can share one role, a member can hold more than
//! one, and a member's effective permissions are the union of every
//! role it holds.
//!
//! Deliberately reuses `Permission` (Read/Propose/Edit/Review/Approve/
//! Execute) rather than inventing a second permission vocabulary,
//! `Specialist` has carried this exact enum since Phase 3, and rounds
//! covering task assignment/review already gate real behavior on it
//! (`Permission::Execute`, `Permission::Review`). `Permission::Approve`
//! existed in that same enum from the start but had never been
//! referenced anywhere in the codebase until this round wires it up,
//! this is that gap closed, not a new concept invented from nothing.
//!
//! Scope, named plainly rather than silently narrowed: this round
//! gives roles to `Member` (every human and AI member, the "everyone"
//! half of the request) and uses them for exactly one concrete gate:
//! `Coordinator::apply_pending_sync_change`/`deny_pending_sync_change`
//! require `Permission::Approve`. `Specialist` keeps its own existing,
//! separately-tested `permissions: Vec<Permission>` field untouched,
//! retrofitting *every* other coordinator action ("everything exactly
//! does as their role says") to check a role is a large, separate
//! undertaking touching dozens of existing methods, not something to
//! fold into the same round as introducing the concept, matching the
//! precedent of every other multi-round feature here (Resilience,
//! GitHub PR mirroring). Named as a follow-up, not left incomplete
//! quietly.

use crate::specialist::Permission;
use crate::Uuid;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub permissions: Vec<Permission>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Role {
    pub fn new(project_id: Uuid, name: impl Into<String>, permissions: Vec<Permission>) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id,
            name: name.into(),
            permissions,
            created_at: chrono::Utc::now(),
        }
    }

    pub fn has_permission(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }
}
