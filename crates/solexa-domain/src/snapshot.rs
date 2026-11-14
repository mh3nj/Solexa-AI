//! ProjectSnapshot, the portable bundle a project syncs through a
//! linked git repository (round 22 built the mechanism scoped to the
//! team roster; round 23 extends it to the Project Brain).
//!
//! Covers Members, Teams, Specialists (+ benchmarks), and the Project
//! Brain's `KnowledgeItem`s and `Decision`s. Two things remain
//! deliberately **excluded**, the same way `TeamPackage` already
//! excludes human membership and hierarchy for its own stated reasons:
//!
//! - **Tasks and events** local execution history tied to whichever
//!   device actually ran a model. Syncing in-flight task state (which
//!   references locally-scheduled resources, not portable config) is a
//!   materially harder problem than syncing configuration or knowledge,
//!   and isn't attempted.
//! - **The local registry** never project-scoped at all (see
//!   `RegistryEntry`'s doc comment), so it has no place in a
//!   per-project snapshot regardless.
//!
//! Round 56 adds a third deliberate exclusion, in the data that *is*
//! carried: **role assignments** (`role_ids` on members and specialists)
//! are device-local policy and never sync, see
//! `ProjectSnapshot::without_role_assignments` for why.
//!
//! Deliberately **no** "exported at" timestamp on the snapshot itself,
//! round 22 had one, and it broke both places this content gets
//! compared for equality (`git`'s own file diff and
//! `Coordinator::git_sync_status`'s dirty-check both misfired, since
//! every export produced different bytes even when nothing had
//! changed). `GitSyncConfig::last_synced_at` already answers "when did
//! this device last agree with the remote", the more meaningful
//! question, so this struct carries no wall-clock field at all.
//!
//! `canonicalize()` sorts every collection by id before it's ever
//! serialized, SQLite doesn't guarantee row order across calls, but
//! two exports of an identical underlying set must produce byte-
//! identical JSON, or the "did anything actually change" checks above
//! would misfire on order alone.

use crate::{Decision, KnowledgeItem, Member, Specialist, SpecialistBenchmark, Team, Uuid};
use serde::{Deserialize, Serialize};

pub const CURRENT_SNAPSHOT_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub format_version: u32,
    pub project_id: Uuid,
    pub project_name: String,
    pub project_description: String,
    pub members: Vec<Member>,
    pub teams: Vec<Team>,
    pub specialists: Vec<Specialist>,
    /// Carried alongside a specialist whenever it's fast-forwarded from
    /// a sync (same reasoning as `SpecialistPackage::benchmark`: a
    /// proven specialist's track record should travel with it). Never
    /// itself part of a sync conflict, see round 22's README on why
    /// benchmarks are additive tallies, not something worth fighting
    /// over.
    pub specialist_benchmarks: Vec<SpecialistBenchmark>,
    /// Round 23: the Project Brain's facts/constraints/artifacts.
    pub knowledge_items: Vec<KnowledgeItem>,
    /// Round 23: the Project Brain's structured decision records.
    pub decisions: Vec<Decision>,
}

impl ProjectSnapshot {
    /// Round 56: blanks every member's and specialist's `role_ids`.
    ///
    /// Role assignments are **device-local policy** and never sync. Role
    /// *definitions* have never travelled in a snapshot, so a `role_id`
    /// names nothing on another device, and if it happens to name a role
    /// that exists there, letting a pull write it would let anyone with
    /// push access to the repo grant `Approve`/`Execute`/`Review` by
    /// naming a local role id. Applied to everything this device writes to
    /// the repo, and to everything it reads back (so a repo written by an
    /// older version, or by hand, is neutralized too).
    pub fn without_role_assignments(mut self) -> Self {
        for member in &mut self.members {
            member.role_ids.clear();
        }
        for specialist in &mut self.specialists {
            specialist.role_ids.clear();
        }
        self
    }

    /// Sorts every collection by id so two exports of the same
    /// underlying state serialize identically regardless of the order
    /// SQLite happened to return rows in. Call this before serializing
    /// or comparing a snapshot, every producer in `solexa-coordinator`
    /// does.
    pub fn canonicalize(mut self) -> Self {
        self.members.sort_by_key(|m| m.id);
        self.teams.sort_by_key(|t| t.id);
        self.specialists.sort_by_key(|s| s.id);
        self.specialist_benchmarks.sort_by_key(|b| b.specialist_id);
        self.knowledge_items.sort_by_key(|k| k.id);
        self.decisions.sort_by_key(|d| d.id);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_roles() -> (ProjectSnapshot, Uuid) {
        let project_id = Uuid::new_v4();
        let role = Uuid::new_v4();
        let mut member = Member::new_human(project_id, "Alex");
        member.role_ids = vec![role, Uuid::new_v4()];
        let mut specialist: Specialist = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "name": "s", "role": "r", "domain": "d", "capabilities": ["general"],
            "preferred_model_ids": [], "tools": [], "knowledge_scope": [], "permissions": ["read"],
            "output_schema": null,
            "escalation_policy": {"escalate_on_weak_confidence": false, "escalate_on_verification_failure": false,
                                  "requires_model_review": false, "requires_specialist_review": false},
            "active": true, "role_ids": [], "max_tokens": 64, "temperature": 0.5
        }))
        .unwrap();
        specialist.role_ids = vec![role];
        let snapshot = ProjectSnapshot {
            format_version: CURRENT_SNAPSHOT_FORMAT_VERSION,
            project_id,
            project_name: "P".into(),
            project_description: String::new(),
            members: vec![member],
            teams: vec![],
            specialists: vec![specialist],
            specialist_benchmarks: vec![],
            knowledge_items: vec![],
            decisions: vec![],
        };
        (snapshot, role)
    }

    #[test]
    fn without_role_assignments_blanks_members_and_specialists_and_nothing_else() {
        let (snapshot, _role) = snapshot_with_roles();
        assert!(!snapshot.members[0].role_ids.is_empty() && !snapshot.specialists[0].role_ids.is_empty(), "precondition: roles present");
        let before = snapshot.clone();

        let after = snapshot.without_role_assignments();
        assert!(after.members[0].role_ids.is_empty());
        assert!(after.specialists[0].role_ids.is_empty());
        // Everything else, including the specialist's own permissions and
        // its limits, which DO sync, is untouched.
        assert_eq!(after.members[0].display_name, before.members[0].display_name);
        assert_eq!(after.members[0].id, before.members[0].id);
        assert_eq!(after.specialists[0].permissions, before.specialists[0].permissions);
        assert_eq!((after.specialists[0].max_tokens, after.specialists[0].temperature), (Some(64), Some(0.5)));
        assert_eq!(after.project_name, before.project_name);
    }

    #[test]
    fn without_role_assignments_is_idempotent_and_makes_snapshots_with_different_roles_equal() {
        let (a, _) = snapshot_with_roles();
        let mut b = a.clone();
        b.members[0].role_ids = vec![Uuid::new_v4()];
        assert_ne!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap(), "precondition: they differ only by role ids");
        let (na, nb) = (a.without_role_assignments(), b.without_role_assignments());
        assert_eq!(serde_json::to_string(&na).unwrap(), serde_json::to_string(&nb).unwrap(), "two devices differing only in role assignments must look identical to sync");
        assert_eq!(serde_json::to_string(&na.clone().without_role_assignments()).unwrap(), serde_json::to_string(&na).unwrap());
    }
}
